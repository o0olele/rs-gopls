use std::path::Path;

use zed_extension_api::{self as zed, settings::LspSettings};

struct RsGoplsExtension;

impl zed::Extension for RsGoplsExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        let local_server = "rs-gopls.exe";
        let command = if Path::new(local_server).is_file() {
            local_server.to_owned()
        } else {
            worktree
                .which("rs-gopls")
                .or_else(|| worktree.which(local_server))
                .ok_or_else(|| {
                    "rs-gopls was not found. Set lsp.rs-gopls.binary.path or add rs-gopls to PATH."
                        .to_owned()
                })?
        };

        Ok(zed::Command {
            command,
            args: Vec::new(),
            env: worktree.shell_env(),
        })
    }

    fn language_server_workspace_configuration(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<Option<zed::serde_json::Value>> {
        Ok(LspSettings::for_worktree(language_server_id.as_ref(), worktree)?.settings)
    }
}

zed::register_extension!(RsGoplsExtension);
