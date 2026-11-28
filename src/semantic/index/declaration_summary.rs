use std::hash::{DefaultHasher, Hash, Hasher};

use tree_sitter::Node;

use super::Source;
use crate::semantic::source::node_text;

// Large generated initializers and deeply nested expressions can remain
// unclassified. An unavailable summary always advances the declaration revision.
const BYTE_LIMIT: usize = 256 * 1024;
const DEPTH_LIMIT: usize = 128;

#[derive(Debug, Eq, PartialEq)]
pub(in crate::semantic) struct DeclarationSummary {
    fingerprint: u64,
    records: Box<[u8]>,
}

impl Hash for DeclarationSummary {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fingerprint.hash(state);
    }
}

impl DeclarationSummary {
    pub(in crate::semantic) fn heap_weight(&self) -> usize {
        self.records.len()
    }

    pub(super) fn collect(source: &Source) -> Option<Self> {
        // The cgo preamble is a comment and can declare entities used by Go.
        if source
            .diagnostic_imports
            .iter()
            .any(|import| import.path.as_deref().or(import.decoded_path.as_deref()) == Some("C"))
        {
            return None;
        }

        let root = source.tree.root_node();
        let mut records = Records(Vec::new());
        let mut seen_package = false;
        let mut seen_declaration = false;
        let mut header_directive = false;
        for node in root.children(&mut root.walk()) {
            if node.is_error() || node.is_missing() {
                return None;
            }
            if !node.is_named() {
                continue;
            }
            match node.kind() {
                "comment" if !seen_package => {
                    header_directive |= directive(node_text(node, &source.text));
                    continue;
                }
                "package_clause" if !seen_package => {
                    seen_package = true;
                    // Build constraint validity can depend on blank lines in
                    // the header, which are absent from the syntax tree.
                    records.text(if header_directive {
                        &source.text[..node.start_byte()]
                    } else {
                        ""
                    })?;
                }
                "comment" | "import_declaration" if seen_package && !seen_declaration => {}
                "function_declaration"
                | "method_declaration"
                | "var_declaration"
                | "const_declaration"
                | "type_declaration"
                    if seen_package =>
                {
                    seen_declaration = true;
                }
                "comment" if seen_package => {}
                // The Go grammar accepts statements at file scope for snippets.
                _ => return None,
            }

            let body = if matches!(node.kind(), "function_declaration" | "method_declaration") {
                node.child_by_field_name("body")
            } else {
                None
            };
            if let Some(body) = body {
                // Interior recovery is safe only when the parser still sees
                // an actual block with both braces and a reliable signature.
                if body.kind() != "block"
                    || !brace(body.child(0), "{")
                    || !brace(body.child(body.child_count().checked_sub(1)?), "}")
                    || node
                        .children(&mut node.walk())
                        .any(|child| child != body && child.has_error())
                {
                    return None;
                }
            } else if node.has_error() {
                return None;
            }

            records.node(node, &source.text, body, 0)?;
        }
        (seen_package && !root.is_error()).then(|| {
            let records = records.0.into_boxed_slice();
            let mut hasher = DefaultHasher::new();
            records.hash(&mut hasher);
            Self {
                fingerprint: hasher.finish(),
                records,
            }
        })
    }
}

fn brace(node: Option<Node<'_>>, kind: &str) -> bool {
    node.is_some_and(|node| !node.is_missing() && node.kind() == kind)
}

fn directive(text: &str) -> bool {
    text.starts_with("//go:")
        || text
            .strip_prefix("//")
            .and_then(|line| line.split_whitespace().next())
            .is_some_and(|word| matches!(word, "go:build" | "+build"))
}

struct Records(Vec<u8>);

impl Records {
    fn node(
        &mut self,
        node: Node<'_>,
        text: &str,
        body: Option<Node<'_>>,
        depth: usize,
    ) -> Option<()> {
        if depth == DEPTH_LIMIT || self.0.len() + 4 > BYTE_LIMIT {
            return None;
        }
        if Some(node) == body {
            self.0.push(0);
            return Some(());
        }
        if depth == 0 && node.kind() == "comment" && !directive(node_text(node, text)) {
            return Some(());
        }

        // Preserve node structure as well as token spelling. This retains the
        // entire initializer of a package variable, including call arguments,
        // result order, type arguments, aliases, and composite literal entries.
        // Only top-level function and method bodies use the single block marker.
        self.0.push(1);
        self.0.extend(node.kind_id().to_le_bytes());
        if node.kind() == "array_type"
            && let Some(length) = node.child_by_field_name("length")
        {
            // The resolver compares array lengths using their source spelling.
            self.text(node_text(length, text))?;
        }
        if node.child_count() == 0 {
            self.0.push(2);
            self.text(node_text(node, text))?;
        } else {
            for child in node.children(&mut node.walk()) {
                self.node(child, text, body, depth + 1)?;
            }
        }
        if self.0.len() == BYTE_LIMIT {
            return None;
        }
        self.0.push(3);
        Some(())
    }

    fn text(&mut self, text: &str) -> Option<()> {
        if text.len() > BYTE_LIMIT.saturating_sub(self.0.len() + 4) {
            return None;
        }
        self.0.extend((text.len() as u32).to_le_bytes());
        self.0.extend_from_slice(text.as_bytes());
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn summary(text: &str) -> Option<DeclarationSummary> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();
        let source = Source::parse(&mut parser, PathBuf::from("p.go"), text.into(), None).unwrap();
        DeclarationSummary::collect(&source)
    }

    fn assert_changed(before: &str, after: &str) {
        assert_ne!(summary(before).unwrap(), summary(after).unwrap());
    }

    #[test]
    fn ignores_function_and_method_bodies_and_declaration_positions() {
        let before = "package p\nfunc f(a int) int { return a }\nfunc (T) m() {}\ntype T struct {}";
        let after = "package p\n\nfunc f(a int) int {\n return a + 1\n}\n\n\
                     func (T) m() { _ = 123 }\n// docs\ntype T struct{}";
        assert_eq!(summary(before).unwrap(), summary(after).unwrap());
    }

    #[test]
    fn tracks_package_imports_build_constraints_and_signatures() {
        let before = "//go:build linux\n\npackage p\nimport d \"dep/path\"\n\
                      type T = d.Value\nconst c = 2\nvar V int\n\
                      func F[T any](t T) int { return 0 }\nfunc (T) m() {}";
        for (old, new) in [
            ("linux", "windows"),
            ("package p", "package p_test"),
            ("import d", "import x"),
            ("dep/path", "dep/other"),
            ("d.Value", "d.Other"),
            ("const c = 2", "const c = 3"),
            ("var V int", "var V string"),
            ("T any", "T comparable"),
            ("F[T", "G[T"),
            ("m()", "m(int)"),
            ("m() {}", "m()"),
        ] {
            assert_changed(before, &before.replace(old, new));
        }
        assert_changed("// +build linux\n\npackage p", "// +build linux\npackage p");
        assert_changed("package p", "// go:build ignore\n\npackage p");
        assert_changed("package p", "package /* comment */ p");
        assert_changed("package p\nvar V [1+2]int", "package p\nvar V [1 + 2]int");
    }

    #[test]
    fn tracks_all_package_initializer_tokens_conservatively() {
        let before = "package p\ntype T = int\n\
                      var first, second = pair[int]()\n\
                      var other = map[string]T{\"key\": 1}\n\
                      var callback = func() int { return 1 }";
        for (old, new) in [
            ("first, second", "second, first"),
            ("pair[int]", "pair[string]"),
            ("type T = int", "type T = string"),
            ("map[string]T", "map[string]string"),
            ("return 1", "return 2"),
        ] {
            assert_changed(before, &before.replace(old, new));
        }
        assert_eq!(
            summary(before).unwrap(),
            summary(&before.replace(" = ", "=")).unwrap()
        );
    }

    #[test]
    fn rejects_unreliable_declarations_and_missing_body_boundaries() {
        for text in [
            "func f() {}",
            "package p\nreturn 1",
            "package p\npackage p2",
            "package p\nfunc f( {}",
            "package p\nfunc f() { return",
            "package p\nvar V =",
            "package p\n/* c preamble */\nimport \"C\"",
        ] {
            assert!(summary(text).is_none(), "unexpected summary for {text:?}");
        }
        assert_eq!(
            summary("package p\nfunc f() { return }").unwrap(),
            summary("package p\nfunc f() { _ = \"🙂\" + ) }").unwrap()
        );
    }
}
