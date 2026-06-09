package com.example;

import org.apache.commons.compress.archivers.zip.ZipArchiveEntry;
import org.apache.commons.compress.archivers.zip.ZipArchiveOutputStream;

import java.io.ByteArrayOutputStream;

/**
 * Demonstrates user-declared exclusion of a transitive dependency
 * (Curie.toml): commons-compress is declared with
 * {@code exclusions = ["commons-codec:*"]}, so the commons-codec JAR — only
 * needed by its LZ4/Snappy compressors — is never placed on the classpath.
 */
public class ExclusionDemo {

    public static void main(String[] args) throws Exception {
        createZipArchive();
        checkUserDeclaredExclusion();
    }

    private static void createZipArchive() throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ZipArchiveOutputStream zip = new ZipArchiveOutputStream(bytes)) {
            zip.putArchiveEntry(new ZipArchiveEntry("hello.txt"));
            zip.write("hello".getBytes());
            zip.closeArchiveEntry();
        }
        System.out.println("ZIP: created " + bytes.size() + "-byte archive.");
    }

    private static void checkUserDeclaredExclusion() {
        try {
            Class.forName("org.apache.commons.codec.binary.Base64");
            System.err.println("ERROR: commons-codec found on classpath — exclusion failed!");
            System.exit(1);
        } catch (ClassNotFoundException expected) {
            System.out.println("ZIP: commons-codec is excluded. ✓");
        }
    }
}
