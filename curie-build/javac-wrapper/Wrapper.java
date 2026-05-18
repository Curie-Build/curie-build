package dev.curie.javac;

import com.sun.source.util.JavacTask;
import com.sun.source.util.TaskEvent;
import com.sun.source.util.TaskListener;

import javax.lang.model.element.TypeElement;
import javax.lang.model.util.Elements;
import javax.tools.JavaCompiler;
import javax.tools.JavaFileObject;
import javax.tools.StandardJavaFileManager;
import javax.tools.ToolProvider;

import java.io.File;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;

/**
 * Drop-in wrapper around the JDK's JavaCompiler that records, at compile
 * time, exactly which class files were produced by each source.
 *
 * Curie invokes this instead of `javac` directly:
 *
 *   java -jar wrapper.jar --curie-manifest-out target/.classes.toml \
 *        --release 21 -g -d target/classes -cp ... src/Foo.java ...
 *
 * After a successful compile the wrapper writes:
 *
 *   [sources]
 *   "/abs/path/to/Foo.java" = ["com/foo/Foo.class", "com/foo/Bar.class"]
 *
 * Curie uses this in two phases:
 *   - pre-compile:  classes whose source has been deleted are stale.
 *   - post-compile: classes whose source still exists but wasn't re-emitted
 *                   by the current build (e.g. removed inner / companion
 *                   class) are stale.
 *
 * The manifest is written ONLY on a successful build so a half-finished
 * compile never poisons the previous good state.
 */
public final class Wrapper {

    public static void main(String[] args) throws IOException {
        Path manifestOut = null;
        List<String> forward = new ArrayList<>();
        for (int i = 0; i < args.length; i++) {
            if ("--curie-manifest-out".equals(args[i]) && i + 1 < args.length) {
                manifestOut = Paths.get(args[++i]);
            } else {
                forward.add(args[i]);
            }
        }

        JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
        if (compiler == null) {
            System.err.println(
                "error: no JavaCompiler available — Curie requires a JDK (not just a JRE)"
            );
            System.exit(2);
        }
        StandardJavaFileManager fm =
            compiler.getStandardFileManager(null, null, StandardCharsets.UTF_8);

        // Split positional .java arguments from javac options.
        List<String> options = new ArrayList<>();
        List<File> sources = new ArrayList<>();
        for (String a : forward) {
            if (a.endsWith(".java")) {
                sources.add(new File(a));
            } else {
                options.add(a);
            }
        }

        Iterable<? extends JavaFileObject> sourceObjs = fm.getJavaFileObjectsFromFiles(sources);
        JavaCompiler.CompilationTask task =
            compiler.getTask(null, fm, null, options, null, sourceObjs);
        JavacTask jcTask = (JavacTask) task;

        // Source-path → produced class-file paths (binary names with $ for nesting).
        Map<String, List<String>> produced = new TreeMap<>();

        jcTask.addTaskListener(new TaskListener() {
            private Elements elements;

            @Override public void finished(TaskEvent e) {
                if (e.getKind() != TaskEvent.Kind.GENERATE) return;
                TypeElement te = e.getTypeElement();
                if (te == null) return;
                if (elements == null) {
                    elements = jcTask.getElements();
                }
                // Binary name uses $ for nested types; replace . with /
                // and append .class to get the class-file path inside -d.
                String binaryName = elements.getBinaryName(te).toString();
                String classRel = binaryName.replace('.', '/') + ".class";

                JavaFileObject src = e.getSourceFile();
                if (src != null) {
                    // toUri().getPath() yields the absolute filesystem path
                    // on Unix; on Windows it has a leading "/C:" which Curie
                    // normalises on its side.
                    String srcPath = src.toUri().getPath();
                    produced
                        .computeIfAbsent(srcPath, k -> new ArrayList<>())
                        .add(classRel);
                }
            }
        });

        boolean ok = task.call();

        if (ok && manifestOut != null) {
            writeManifest(manifestOut, produced);
        }
        System.exit(ok ? 0 : 1);
    }

    private static void writeManifest(Path out, Map<String, List<String>> produced)
            throws IOException {
        StringBuilder sb = new StringBuilder(256 + produced.size() * 64);
        sb.append("# Authoritative source → class-file mapping from the\n");
        sb.append("# last successful compile, written by curie-javac-wrapper.\n");
        sb.append("# Do not edit by hand; rerun `curie build` to regenerate.\n\n");
        sb.append("[sources]\n");
        for (Map.Entry<String, List<String>> e : produced.entrySet()) {
            sb.append(quoteTomlKey(e.getKey())).append(" = [");
            List<String> classes = e.getValue();
            for (int i = 0; i < classes.size(); i++) {
                if (i > 0) sb.append(", ");
                sb.append(quoteTomlBasic(classes.get(i)));
            }
            sb.append("]\n");
        }
        Path parent = out.getParent();
        if (parent != null) {
            Files.createDirectories(parent);
        }
        // Atomic-ish write: temp file then rename so a crash mid-write
        // can't truncate the previous good manifest.
        Path tmp = out.resolveSibling(out.getFileName().toString() + ".part");
        Files.writeString(tmp, sb.toString(), StandardCharsets.UTF_8);
        Files.move(tmp, out, java.nio.file.StandardCopyOption.REPLACE_EXISTING);
    }

    /** TOML quoted key: escape backslash and double-quote, wrap in "". */
    private static String quoteTomlKey(String s) {
        return quoteTomlBasic(s);
    }

    /** TOML basic string: escape backslash and double-quote, wrap in "". */
    private static String quoteTomlBasic(String s) {
        StringBuilder sb = new StringBuilder(s.length() + 2);
        sb.append('"');
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c == '\\' || c == '"') {
                sb.append('\\');
            }
            sb.append(c);
        }
        sb.append('"');
        return sb.toString();
    }

    private Wrapper() {}
}
