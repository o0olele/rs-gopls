import com.google.gson.GsonBuilder
import com.intellij.ide.hierarchy.HierarchyNodeDescriptor
import com.intellij.ide.plugins.ContentModuleDescriptor
import com.intellij.ide.plugins.PluginManagerCore
import com.intellij.openapi.actionSystem.CommonDataKeys
import com.intellij.openapi.actionSystem.DataContext
import com.intellij.openapi.application.ApplicationInfo
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.command.WriteCommandAction
import com.intellij.openapi.command.undo.UndoUtil
import com.intellij.openapi.editor.Document
import com.intellij.openapi.fileEditor.FileDocumentManager
import com.intellij.openapi.fileEditor.FileDocumentSynchronizationVetoer
import com.intellij.openapi.module.ModuleUtilCore
import com.intellij.openapi.progress.EmptyProgressIndicator
import com.intellij.openapi.progress.ProgressManager
import com.intellij.openapi.project.DumbService
import com.intellij.openapi.project.ProjectManager
import com.intellij.openapi.util.Computable
import com.intellij.openapi.util.Disposer
import com.intellij.openapi.util.TextRange
import com.intellij.openapi.vfs.LocalFileSystem
import com.intellij.psi.PsiDocumentManager
import com.intellij.psi.PsiElement
import com.intellij.psi.PsiManager

import java.nio.file.Files
import java.nio.file.InvalidPathException
import java.nio.file.Path
import java.nio.file.StandardCopyOption

class HierarchyEditSaveVeto extends FileDocumentSynchronizationVetoer {
    final Document document

    HierarchyEditSaveVeto(Document document) { this.document = document }

    @Override
    boolean maySaveDocument(Document candidate, boolean explicit) { !document.is(candidate) }
}

def directory = Path.of(System.getenv('TEMP') ?: System.getProperty('java.io.tmpdir'), 'rs-gopls-goland-baseline')
def gson = new GsonBuilder().disableHtmlEscaping().serializeNulls().setPrettyPrinting().create()
def spec = gson.fromJson(Files.readString(directory.resolve('hierarchy-queries.json')), Map)
Path root
try {
    root = directory.resolve(spec.root as String).toRealPath()
} catch (IOException | InvalidPathException ignored) {
    throw new IllegalArgumentException('The configured hierarchy fixture directory is unavailable')
}
def build = ApplicationInfo.instance.build.asString()
if (build != spec.build) throw new IllegalStateException("Expected $spec.build, found $build")
def project = ProjectManager.instance.openProjects.find {
    try {
        it.basePath && Path.of(it.basePath).toRealPath() == root
    } catch (IOException | InvalidPathException ignored) {
        false
    }
}
if (!project) {
    throw new IllegalStateException(
        'Open the target fixture directory as a project in the running GoLand instance and wait for indexing to finish before capture')
}

def goModule = PluginManagerCore.pluginSet.enabledModules.find {
    it instanceof ContentModuleDescriptor && it.moduleNameString == 'intellij.go.impl'
}
if (!goModule) throw new IllegalStateException('GoLand Go implementation module is unavailable')
def loader = goModule.pluginClassLoader
def callProvider = loader.loadClass('com.goide.hierarchy.GoCallHierarchyProvider').getDeclaredConstructor().newInstance()
def typeProvider = loader.loadClass('com.goide.hierarchy.GoTypeHierarchyProvider').getDeclaredConstructor().newInstance()
def callerTree = loader.loadClass('com.goide.hierarchy.GoCallerHierarchyTreeStructure')
def calleeTree = loader.loadClass('com.goide.hierarchy.GoCalleeHierarchyTreeStructure')
def typeTree = loader.loadClass('com.goide.hierarchy.GoTypesHierarchyTreeStructure')
def superSearch = loader.loadClass('com.goide.go.GoGotoSuperHandler').getField('SUPER_SEARCH').get(null)
def inheritorsSearch = loader.loadClass('com.goide.go.GoInheritorsSearch').getField('INHERITORS_SEARCH').get(null)
// Call nodes store the exact sites the hierarchy browser highlights as smart pointers.
def references = loader.loadClass('com.goide.hierarchy.GoCallHierarchyNodeDescriptor').getDeclaredField('myReferencePointers')
references.setAccessible(true)
def sdkService = loader.loadClass('com.goide.sdk.GoSdkService').getInstance(project)
def documents = FileDocumentManager.instance
def commits = PsiDocumentManager.getInstance(project)
def psi = PsiManager.getInstance(project)
def dumb = DumbService.getInstance(project)
def benchmark = System.getenv('RS_GOPLS_BENCHMARK') == '1'
def configuredIterations = spec.iterations == null ? 30 : spec.iterations
if (!(configuredIterations instanceof Number) || configuredIterations != configuredIterations.intValue() ||
    configuredIterations.intValue() < 1) {
    throw new IllegalArgumentException('Expected a positive integer for iterations')
}
int iterations = configuredIterations.intValue()
String editPrefix = '// rs-gopls benchmark overlay\n'
def scope = 'All'

def application = ApplicationManager.application
def edt = { Closure action ->
    if (application.dispatchThread) action()
    else application.invokeAndWait(action as Runnable)
}
def fixturePath = { String input, String id ->
    try {
        Path relative = Path.of(input)
        if (relative.absolute) throw new IllegalArgumentException("Expected a relative path for $id")
        Path path = root.resolve(relative).toRealPath()
        if (!path.startsWith(root)) throw new IllegalArgumentException("Source is outside the fixture for $id")
        path
    } catch (IOException | InvalidPathException ignored) {
        throw new IllegalArgumentException("Fixture file is unavailable for $id")
    }
}
def relativePath = { Path path -> root.relativize(path).toString().replace('\\', '/') }

def position = { Document document, int offset, int shift ->
    int line = document.getLineNumber(offset)
    [line: line - shift, character: offset - document.getLineStartOffset(line)]
}
def range = { Document document, TextRange span, Document editedDocument ->
    int shift = document.is(editedDocument) ? 1 : 0
    [start: position(document, span.startOffset, shift), end: position(document, span.endOffset, shift)]
}
def itemFor = { PsiElement element, Document editedDocument ->
    def file = element.containingFile?.virtualFile
    if (!file?.inLocalFileSystem) return null
    Path path = Path.of(file.path).normalize()
    if (!path.startsWith(root)) return null
    def document = documents.getDocument(file)
    def identifier = element.identifier
    if (!document || !identifier || (!document.is(editedDocument) && documents.isDocumentUnsaved(document))) {
        throw new IllegalStateException("Expected a declaration in a saved or benchmark-edited document for $element.name")
    }
    [name: element.name, psi: element.class.simpleName.replaceFirst(/Impl$/, ''),
     path: relativePath(path), range: range(document, element.textRange, editedDocument),
     selectionRange: range(document, identifier.textRange, editedDocument)]
}
def comparePosition = { a, b -> (a.line <=> b.line) ?: (a.character <=> b.character) }
def compareItems = { a, b ->
    (a.path <=> b.path) ?: comparePosition(a.selectionRange.start, b.selectionRange.start)
}

def diskState = { Path path, String id ->
    try {
        [Files.readAllBytes(path), Files.getLastModifiedTime(path)]
    } catch (IOException ignored) {
        throw new IllegalStateException("Cannot read benchmark fixture on disk for $id")
    }
}
def captureAfterEdit = { Path path, Document document, String id, Closure measure, Map expected ->
    def (original, originalTimestamp) = diskState(path, id)
    def editLifetime = Disposer.newDisposable('GoLand hierarchy edit save veto')
    try {
        String savedText
        edt {
            if (!document || documents.isDocumentUnsaved(document) || commits.isUncommited(document)) {
                throw new IllegalStateException("Expected a saved, committed edit document for $id")
            }
            savedText = document.text
            FileDocumentSynchronizationVetoer.EP_NAME.point.registerExtension(
                new HierarchyEditSaveVeto(document), editLifetime)
        }
        // IntelliJ normalizes document line endings to LF, including for CRLF files.
        (0..<iterations).collect {
            def elapsed
            try {
                edt {
                    UndoUtil.disableUndoIn(document, {
                        WriteCommandAction.runWriteCommandAction(project, {
                            document.insertString(0, editPrefix)
                            commits.commitDocument(document)
                        } as Runnable)
                    } as Runnable)
                    if (document.text != editPrefix + savedText ||
                        !documents.isDocumentUnsaved(document) || commits.isUncommited(document)) {
                        throw new IllegalStateException("Benchmark edit did not stay unsaved and committed for $id")
                    }
                }
                def (ms, observed) = measure(document)
                if (observed != expected) throw new IllegalStateException("Hierarchy changed after editing for $id")
                elapsed = ms
            } finally {
                edt {
                    if (document.text != savedText || documents.isDocumentUnsaved(document)) {
                        UndoUtil.disableUndoIn(document, { documents.reloadFromDisk(document, project) } as Runnable)
                    }
                    commits.commitDocument(document)
                    if (document.text != savedText || documents.isDocumentUnsaved(document) ||
                        commits.isUncommited(document)) {
                        throw new IllegalStateException("Benchmark buffer did not restore for $id")
                    }
                }
                def (bytes, timestamp) = diskState(path, id)
                if (!Arrays.equals(original, bytes) || timestamp != originalTimestamp) {
                    throw new IllegalStateException("Benchmark fixture changed on disk for $id")
                }
            }
            if (measure()[1] != expected) throw new IllegalStateException("Hierarchy changed after restoring for $id")
            elapsed
        }
    } finally {
        edt { Disposer.dispose(editLifetime) }
    }
}

def laterCaptures = []
def firstCaptures = spec.cases.collect { query ->
    Path path = fixturePath(query.path as String, query.id as String)
    String normalized = relativePath(path)
    def file = LocalFileSystem.instance.refreshAndFindFileByNioFile(path)
    def (source, sourceDocument, offset, sdkVersion) = dumb.runReadActionInSmartMode({
        def parsed = file ? psi.findFile(file) : null
        def doc = parsed ? documents.getDocument(file) : null
        if (!doc || documents.isDocumentUnsaved(doc)) {
            throw new IllegalStateException("Expected a saved source document: $normalized")
        }
        int line = query.line
        int character = query.character
        if (line < 0 || line >= doc.lineCount || character < 0 ||
            character >= doc.getLineEndOffset(line) - doc.getLineStartOffset(line)) {
            throw new IllegalArgumentException("Invalid position for $query.id")
        }
        def sdk = sdkService.getSdk(ModuleUtilCore.findModuleForFile(file, project))
        if (!sdk.version) throw new IllegalStateException("Configure a Go SDK for $normalized before capture")
        [parsed, doc, doc.getLineStartOffset(line) + character, sdk.version]
    } as Computable)

    boolean call = query.request in ['textDocument/prepareCallHierarchy', 'callHierarchy/incomingCalls', 'callHierarchy/outgoingCalls']
    def measure = { Document editedDocument = null ->
        int queryOffset = offset + (sourceDocument.is(editedDocument) ? editPrefix.length() : 0)
        def indicator = new EmptyProgressIndicator()
        long start = System.nanoTime()
        def result = ProgressManager.instance.runProcess({
            dumb.runReadActionInSmartMode({
                def parsed = source.valid ? source : psi.findFile(file)
                def leaf = parsed?.findElementAt(queryOffset)
                if (leaf?.text != query.token) throw new IllegalStateException("Unexpected token for $query.id")
                def candidate = parsed.findReferenceAt(queryOffset)?.resolve() ?: leaf
                def context = { String key ->
                    if (CommonDataKeys.PROJECT.is(key)) return project
                    if (CommonDataKeys.PSI_ELEMENT.is(key)) return candidate
                    null
                } as DataContext
                def target = (call ? callProvider : typeProvider).getTarget(context)
                def item = target ? itemFor(target, editedDocument) : null
                if (!item) throw new IllegalStateException("No in-fixture hierarchy target for $query.id")

                def tree
                String edge = null
                switch (query.request) {
                    case 'textDocument/prepareCallHierarchy':
                    case 'textDocument/prepareTypeHierarchy':
                        return [target: item, results: [item]]
                    case 'callHierarchy/incomingCalls':
                        tree = callerTree.newInstance(target, scope, false)
                        edge = 'from'
                        break
                    case 'callHierarchy/outgoingCalls':
                        tree = calleeTree.newInstance(target, scope, false)
                        edge = 'to'
                        break
                    case 'typeHierarchy/supertypes':
                        tree = typeTree.newInstance(target, superSearch, scope)
                        break
                    case 'typeHierarchy/subtypes':
                        tree = typeTree.newInstance(target, inheritorsSearch, scope)
                        break
                    default:
                        throw new IllegalArgumentException("Unsupported hierarchy request: $query.request")
                }

                def items = tree.getChildElements(tree.baseDescriptor).findResults { HierarchyNodeDescriptor node ->
                    def child = itemFor(node.psiElement, editedDocument)
                    if (!child || !edge) return child
                    def sites = references.get(node).collect { pointer ->
                        def site = pointer.element
                        def siteDocument = documents.getDocument(site.containingFile.virtualFile)
                        range(siteDocument, site.textRange, editedDocument)
                    }.unique().sort { a, b ->
                        comparePosition(a.start, b.start) ?: comparePosition(a.end, b.end)
                    }
                    [(edge): child, fromRanges: sites]
                }.unique().sort { a, b -> edge ? compareItems(a[edge], b[edge]) : compareItems(a, b) }
                [target: item, results: items]
            } as Computable)
        } as Computable, indicator)
        [(System.nanoTime() - start) / 1_000_000.0d, result]
    }

    def captureFirst = {
        def (firstMs, result) = measure()
        def row = [id: query.id, request: query.request, path: normalized, line: query.line as int,
                   character: query.character as int, sdkVersion: sdkVersion, target: result.target,
                   results: result.results, firstMs: firstMs, timingsMs: []]
        if (benchmark) {
            laterCaptures << {
                5.times {
                    if (measure()[1] != result) throw new IllegalStateException("Hierarchy changed for $query.id")
                }
                iterations.times {
                    def (ms, observed) = measure()
                    if (observed != result) throw new IllegalStateException("Hierarchy changed for $query.id")
                    row.timingsMs << ms
                }
                Path editPath = query.editFile == null ? path : fixturePath(query.editFile as String, query.id as String)
                def editDocument = sourceDocument
                if (editPath != path) {
                    row.editFile = relativePath(editPath)
                    def editFile = LocalFileSystem.instance.refreshAndFindFileByNioFile(editPath)
                    editDocument = dumb.runReadActionInSmartMode({
                        editFile && psi.findFile(editFile) ? documents.getDocument(editFile) : null
                    } as Computable)
                }
                row.afterEditTimingsMs = captureAfterEdit(editPath, editDocument, query.id as String, measure, result)
            }
        }
        row
    }
    captureFirst
}

// Load all source documents and collect all first queries before warmups or edits.
def cases = firstCaptures.collect { it() }
laterCaptures.each { it() }
def capture = [schemaVersion: 1, build: build, root: '.', scope: scope, cases: cases]
if (benchmark) capture.editPrefix = editPrefix
def pending = directory.resolve('goland-hierarchy.json.pending')
Files.writeString(pending, gson.toJson(capture) + '\n')
Files.move(pending, directory.resolve('goland-hierarchy.json'), StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE)
