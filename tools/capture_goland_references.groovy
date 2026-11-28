import com.intellij.find.findUsages.FindUsagesHandlerFactory
import com.intellij.find.findUsages.FindUsagesManager
import com.intellij.openapi.application.ApplicationInfo
import com.intellij.openapi.application.ReadAction
import com.intellij.openapi.fileEditor.FileDocumentManager
import com.intellij.openapi.progress.EmptyProgressIndicator
import com.intellij.openapi.progress.ProgressManager
import com.intellij.openapi.project.DumbService
import com.intellij.openapi.project.ProjectManager
import com.intellij.openapi.util.Computable
import com.intellij.openapi.util.ThrowableComputable
import com.intellij.openapi.vfs.LocalFileSystem
import com.intellij.psi.PsiComment
import com.intellij.psi.PsiManager
import com.intellij.psi.util.PsiTreeUtil
import com.intellij.psi.PsiNameIdentifierOwner
import com.intellij.psi.search.GlobalSearchScope
import com.intellij.usages.UsageInfo2UsageAdapter
import com.intellij.util.Processor
import com.intellij.openapi.actionSystem.DataContext

import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.StandardCopyOption

def directory = Path.of(System.getenv('TEMP'), 'rs-gopls-goland-baseline')
def input = Files.readAllLines(directory.resolve('reference-queries.tsv'))
def root = Path.of(input[0]).toRealPath()
def queries = input.drop(1)
int iterations = 0
if (queries && queries[0].startsWith('# iterations\t')) {
    iterations = queries[0].split('\t', -1)[1].toInteger()
    if (iterations < 1) throw new IllegalArgumentException('Expected positive reference timing iterations')
    queries = queries.drop(1)
}
def project = ProjectManager.instance.openProjects.find {
    it.basePath && Path.of(it.basePath).toRealPath() == root
}
if (!project) throw new IllegalStateException("Open the requested GoLand project first: $root")

def files = LocalFileSystem.instance
def docs = FileDocumentManager.instance
def psi = PsiManager.getInstance(project)
def dumb = DumbService.getInstance(project)
def manager = new FindUsagesManager(project)

def coordinates = { doc, int offset ->
    def line = doc.getLineNumber(offset)
    [line, offset - doc.getLineStartOffset(line)]
}

def build = ApplicationInfo.instance.build.asString()
def rows = [build]
def timings = [build, 'id\tincludeDeclaration\titeration\telapsedMs\tsourceCount\tideCount']
queries.each { row ->
    def (id, relative, line, character, token) = row.split('\t', -1)
    def (handler, options, declaration) = dumb.runReadActionInSmartMode({
        def file = files.findFileByNioFile(root.resolve(relative))
        def doc = docs.getDocument(file)
        if (!doc || docs.isDocumentUnsaved(doc)) throw new IllegalStateException("Save query first: $relative")
        def source = psi.findFile(file)
        def offset = doc.getLineStartOffset(line.toInteger()) + character.toInteger()
        def leaf = source.findElementAt(offset)
        if (leaf?.text != token) throw new IllegalStateException("Unexpected token in $id")
        def target = source.findReferenceAt(offset)?.resolve()
        if (!target) {
            for (def owner = leaf.parent; owner != null; owner = owner.parent) {
                if (owner instanceof PsiNameIdentifierOwner && owner.nameIdentifier == leaf) {
                    target = owner
                    break
                }
            }
        }
        if (!(target instanceof PsiNameIdentifierOwner)) {
            throw new IllegalStateException("No supported declaration for $id ($target)")
        }
        FindUsagesHandlerFactory.OperationMode direct = FindUsagesHandlerFactory.OperationMode.HIGHLIGHT_USAGES
        def found = manager.getFindUsagesHandler(target, direct)
        if (!found) throw new IllegalStateException("No Find Usages handler for $id")
        def settings = found.getFindUsagesOptions(DataContext.EMPTY_CONTEXT).clone()
        settings.searchScope = GlobalSearchScope.projectScope(project)
        settings.isUsages = true
        settings.isSearchForTextOccurrences = false
        def named = target.nameIdentifier
        def targetFile = named.containingFile.virtualFile
        def targetDoc = docs.getDocument(targetFile)
        if (!targetDoc || docs.isDocumentUnsaved(targetDoc)) throw new IllegalStateException("Save declaration first: $id")
        def range = named.textRange
        def declaration = ['declaration', id, targetFile.path,
            *coordinates(targetDoc, range.startOffset), *coordinates(targetDoc, range.endOffset), named.text]
        [found, settings, declaration]
    } as Computable)

    def measure = { boolean includeDeclaration ->
        long started = System.nanoTime()
        def searcher = FindUsagesManager.createUsageSearcher(
            handler, handler.primaryElements, handler.secondaryElements, options
        )
        def usages = Collections.synchronizedSet(new TreeSet<String>())
        ProgressManager.instance.runProcess({
            searcher.generate({ usage ->
                if (!(usage instanceof UsageInfo2UsageAdapter)) {
                    throw new IllegalStateException("Unsupported usage for $id: " + usage.class.name)
                }
                def observation = ReadAction.compute({
                    def info = usage.usageInfo
                    def file = info.virtualFile
                    def doc = file ? docs.getDocument(file) : null
                    def range = info.navigationRange
                    if (!doc || docs.isDocumentUnsaved(doc) || !range ||
                        range.startOffset < 0 || range.endOffset <= range.startOffset) {
                        throw new IllegalStateException("No saved and precise usage range for $id")
                    }
                    def leaf = info.file?.findElementAt(range.startOffset)
                    def comment = PsiTreeUtil.getParentOfType(leaf, PsiComment, false)
                    [comment || info.nonCodeUsage ? 'text' : 'usage', id, file.path,
                        *coordinates(doc, range.startOffset), *coordinates(doc, range.endOffset),
                        doc.charsSequence.subSequence(range.startOffset, range.endOffset).toString()
                            .replace('\t', '\\t').replace('\n', '\\n')].join('\t')
                } as ThrowableComputable)
                usages.add(observation)
                true
            } as Processor)
        } as Runnable, new EmptyProgressIndicator())
        if (includeDeclaration) usages.add(declaration.join('\t'))
        [usages, (System.nanoTime() - started) / 1_000_000.0d]
    }
    def captured = measure(false)[0]
    def declarationRow = declaration.join('\t')
    rows << declarationRow
    rows.addAll(captured)
    iterations.times { index ->
        (index % 2 == 0 ? [false, true] : [true, false]).each { includeDeclaration ->
            def (current, elapsed) = measure(includeDeclaration)
            def expected = includeDeclaration ? captured + [declarationRow] : captured
            if (current != expected) {
                throw new IllegalStateException("Reference set drift for $id at iteration ${index + 1}")
            }
            def ideCount = current.count { it.startsWith('text\t') }
            timings << [id, includeDeclaration, index + 1, elapsed, current.size() - ideCount, ideCount].join('\t')
        }
    }
}

def output = directory.resolve('reference-locations.tsv')
def pending = directory.resolve('reference-locations.pending')
Files.write(pending, rows)
Files.move(pending, output, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE)
if (iterations) {
    def timedOutput = directory.resolve('reference-timings.tsv')
    def timedPending = directory.resolve('reference-timings.pending')
    Files.write(timedPending, timings)
    Files.move(timedPending, timedOutput, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE)
}
