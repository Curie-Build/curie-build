package com.curie.runner;

import org.junit.platform.engine.TestExecutionResult;
import org.junit.platform.engine.discovery.ClassNameFilter;
import org.junit.platform.engine.discovery.DiscoverySelectors;
import org.junit.platform.engine.support.descriptor.MethodSource;
import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.TestIdentifier;
import org.junit.platform.launcher.TestExecutionListener;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.core.LauncherFactory;

import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;

/**
 * Thin JUnit Platform Launcher wrapper invoked by Curie instead of the
 * console-standalone JAR.  Captures per-test stdout and writes results to
 * JSON + individual output text files.
 *
 * Args:
 *   args[0] — path for the JSON results file   (target/build.tests.json)
 *   args[1] — directory for per-test text files (target/test-output, pre-created)
 *   args[2..] — optional --include-classname=<pattern>
 *               optional --exclude-classname=<pattern>
 */
public class CurieTestRunner {

    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            System.err.println("Usage: CurieTestRunner <json-out> <output-dir> [--include-classname=...] [--exclude-classname=...]");
            System.exit(2);
        }

        Path jsonOut   = Path.of(args[0]);
        Path outputDir = Path.of(args[1]);
        List<String> includePatterns = new ArrayList<>();
        List<String> excludePatterns = new ArrayList<>();
        for (int i = 2; i < args.length; i++) {
            if (args[i].startsWith("--include-classname=")) {
                includePatterns.add(args[i].substring("--include-classname=".length()));
            } else if (args[i].startsWith("--exclude-classname=")) {
                excludePatterns.add(args[i].substring("--exclude-classname=".length()));
            }
        }

        Set<Path> classpathRoots = classpathRoots();

        LauncherDiscoveryRequestBuilder requestBuilder =
            LauncherDiscoveryRequestBuilder.request()
                .selectors(DiscoverySelectors.selectClasspathRoots(classpathRoots));

        if (!includePatterns.isEmpty()) {
            requestBuilder.filters(
                ClassNameFilter.includeClassNamePatterns(includePatterns.toArray(new String[0])));
        }
        if (!excludePatterns.isEmpty()) {
            requestBuilder.filters(
                ClassNameFilter.excludeClassNamePatterns(excludePatterns.toArray(new String[0])));
        }

        LauncherDiscoveryRequest request = requestBuilder.build();
        ResultListener listener = new ResultListener(outputDir);

        LauncherFactory.create().execute(request, listener);

        listener.printSummary();
        listener.writeJson(jsonOut);
        System.exit(listener.anyFailed() ? 1 : 0);
    }

    private static Set<Path> classpathRoots() {
        Set<Path> roots = new LinkedHashSet<>();
        String cp = System.getProperty("java.class.path", "");
        for (String entry : cp.split(File.pathSeparator)) {
            if (entry.isBlank()) continue;
            Path p = Path.of(entry);
            if (!Files.isDirectory(p)) continue;
            // Only scan test output dirs. Production classes stay on the
            // runtime classpath but must not be discovered: a testing
            // library (Guava testlib) ships TestCase subclasses that
            // Surefire never launches as standalone tests.
            if (!"test-classes".equals(p.getFileName().toString())) continue;
            roots.add(p.toAbsolutePath().normalize());
        }
        return roots;
    }
}

class ResultListener implements TestExecutionListener {

    record TestRecord(
        String name,
        String className,
        long   durationMs,
        String status,
        String failure,
        String outputFile
    ) {}

    private final Path              outputDir;
    private final PrintStream       originalOut = System.out;
    private final List<TestRecord>  results     = new ArrayList<>();
    private final Map<String, Long> startTimes  = new HashMap<>();
    private final ByteArrayOutputStream captureBuf = new ByteArrayOutputStream(4096);
    private String currentId;

    ResultListener(Path outputDir) {
        this.outputDir = outputDir;
        // One tee for the whole run. Allocating a PrintStream per test OOMs
        // on suites that expand to tens of thousands of methods (Guava).
        System.setOut(new PrintStream(new TeeOutputStream(originalOut, captureBuf), true, StandardCharsets.UTF_8));
    }

    @Override
    public void executionStarted(TestIdentifier id) {
        if (!id.isTest()) return;
        startTimes.put(id.getUniqueId(), System.currentTimeMillis());
        captureBuf.reset();
        currentId = id.getUniqueId();
    }

    @Override
    public void executionFinished(TestIdentifier id, TestExecutionResult result) {
        if (!id.isTest()) return;
        System.out.flush();

        Long started = startTimes.remove(id.getUniqueId());
        long duration = started == null ? 0 : System.currentTimeMillis() - started;

        String captured = captureBuf.size() == 0 ? "" : captureBuf.toString(StandardCharsets.UTF_8);
        captureBuf.reset();
        if (id.getUniqueId().equals(currentId)) {
            currentId = null;
        }

        String outputFile = null;
        if (!captured.isBlank()) {
            outputFile = persistOutput(id, captured);
        }

        String status = switch (result.getStatus()) {
            case SUCCESSFUL -> "passed";
            case FAILED     -> "failed";
            case ABORTED    -> "skipped";
        };

        String failure = result.getThrowable()
            .map(Throwable::toString)
            .orElse(null);

        results.add(new TestRecord(
            id.getDisplayName(),
            extractClassName(id),
            duration,
            status,
            failure,
            outputFile
        ));
    }

    @Override
    public void executionSkipped(TestIdentifier id, String reason) {
        results.add(new TestRecord(
            id.getDisplayName(),
            extractClassName(id),
            0L,
            "skipped",
            reason,
            null
        ));
    }

    boolean anyFailed() {
        return results.stream().anyMatch(r -> "failed".equals(r.status()) || "errored".equals(r.status()));
    }

    void printSummary() {
        System.setOut(originalOut);
        long passed  = results.stream().filter(r -> "passed".equals(r.status())).count();
        long failed  = results.stream().filter(r -> "failed".equals(r.status())).count();
        long skipped = results.stream().filter(r -> "skipped".equals(r.status())).count();

        // Print each failure with its message.
        for (TestRecord r : results) {
            if (!"failed".equals(r.status())) continue;
            originalOut.println();
            originalOut.println("FAILED  " + r.className() + " > " + r.name());
            if (r.failure() != null) {
                // Print each line of the failure message indented.
                for (String line : r.failure().split("\n")) {
                    originalOut.println("  " + line);
                }
            }
        }

        // Final count line.
        originalOut.println();
        StringBuilder summary = new StringBuilder();
        summary.append(passed).append(" passed");
        if (failed  > 0) summary.append(", ").append(failed).append(" failed");
        if (skipped > 0) summary.append(", ").append(skipped).append(" skipped");
        originalOut.println(summary);
        originalOut.flush();
    }

    void writeJson(Path path) throws IOException {
        Files.createDirectories(path.getParent() == null ? Path.of(".") : path.getParent());
        StringBuilder sb = new StringBuilder("[\n");
        for (int i = 0; i < results.size(); i++) {
            TestRecord r = results.get(i);
            sb.append("  {\n");
            appendField(sb, "name",        r.name(),       true);
            appendField(sb, "class_name",  r.className(),  true);
            sb.append("    \"duration_ms\": ").append(r.durationMs()).append(",\n");
            appendField(sb, "status",      r.status(),     true);
            appendNullableField(sb, "failure",     r.failure(),    true);
            appendNullableField(sb, "output_file", r.outputFile(), false);
            sb.append("  }");
            if (i < results.size() - 1) sb.append(",");
            sb.append("\n");
        }
        sb.append("]");
        Files.writeString(path, sb.toString(), StandardCharsets.UTF_8);
    }

    // -----------------------------------------------------------------------

    private String persistOutput(TestIdentifier id, String content) {
        String className = extractClassName(id);
        String fileName  = outputFileName(id) + ".txt";
        // class_name → directory: "com.example.Foo" → "com/example/Foo"
        Path classDir = outputDir.resolve(className.replace('.', '/'));
        Path outPath  = classDir.resolve(fileName);
        try {
            Files.createDirectories(classDir);
            Files.writeString(outPath, content, StandardCharsets.UTF_8);
            // Return path relative to outputDir's parent (project target root's parent)
            return outputDir.getParent() != null
                ? outputDir.getParent().relativize(outPath).toString().replace('\\', '/')
                : outPath.toString().replace('\\', '/');
        } catch (IOException e) {
            System.err.println("Warning: could not write test output: " + e.getMessage());
            return null;
        }
    }

    private static String extractClassName(TestIdentifier id) {
        return id.getSource()
            .filter(s -> s instanceof MethodSource)
            .map(s -> ((MethodSource) s).getClassName())
            .orElse("");
    }

    /**
     * Derive a filesystem-safe, unique filename from the last segment of the
     * JUnit unique ID.  For a regular test `[method:batch_01()]` this gives
     * `method_batch_01`.  For a parameterized invocation
     * `[test-template-invocation:#3]` this gives `test-template-invocation_3`.
     */
    private static String outputFileName(TestIdentifier id) {
        String uid = id.getUniqueId();
        // Last segment is "[kind:value]"
        int lastSlash = uid.lastIndexOf('/');
        String segment = lastSlash >= 0 ? uid.substring(lastSlash + 1) : uid;
        // Strip surrounding []
        if (segment.startsWith("[") && segment.endsWith("]")) {
            segment = segment.substring(1, segment.length() - 1);
        }
        // Replace non-alphanumeric/dash characters with _
        segment = segment.replaceAll("[^a-zA-Z0-9_\\-]", "_");
        // Collapse consecutive underscores
        segment = segment.replaceAll("_+", "_");
        // Trim leading/trailing underscores
        segment = segment.replaceAll("^_+|_+$", "");
        return segment.isEmpty() ? "test" : segment;
    }

    private static void appendField(StringBuilder sb, String key, String value, boolean comma) {
        sb.append("    \"").append(key).append("\": ").append(jsonString(value));
        if (comma) sb.append(",");
        sb.append("\n");
    }

    private static void appendNullableField(StringBuilder sb, String key, String value, boolean comma) {
        sb.append("    \"").append(key).append("\": ");
        if (value == null) {
            sb.append("null");
        } else {
            sb.append(jsonString(value));
        }
        if (comma) sb.append(",");
        sb.append("\n");
    }

    private static String jsonString(String s) {
        if (s == null) return "null";
        StringBuilder out = new StringBuilder("\"");
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"'  -> out.append("\\\"");
                case '\\' -> out.append("\\\\");
                case '\n' -> out.append("\\n");
                case '\r' -> out.append("\\r");
                case '\t' -> out.append("\\t");
                default   -> {
                    if (c < 0x20) {
                        out.append(String.format("\\u%04x", (int) c));
                    } else {
                        out.append(c);
                    }
                }
            }
        }
        out.append("\"");
        return out.toString();
    }
}

class TeeOutputStream extends OutputStream {

    private final OutputStream first;
    private final OutputStream second;

    TeeOutputStream(OutputStream first, OutputStream second) {
        this.first  = first;
        this.second = second;
    }

    @Override
    public void write(int b) throws IOException {
        first.write(b);
        second.write(b);
    }

    @Override
    public void write(byte[] b, int off, int len) throws IOException {
        first.write(b, off, len);
        second.write(b, off, len);
    }

    @Override
    public void flush() throws IOException {
        first.flush();
        second.flush();
    }
}
