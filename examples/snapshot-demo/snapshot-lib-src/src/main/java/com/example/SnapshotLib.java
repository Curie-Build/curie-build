package com.example;

/**
 * Trivial library published as a unique Maven snapshot for curie's
 * snapshot-demo. Change the {@link #greet(String)} message and re-run
 * {@code mvn deploy} to mint a newer snapshot build for testing
 * {@code curie build -U}.
 */
public final class SnapshotLib {

    private SnapshotLib() {
    }

    public static String greet(String name) {
        return "Hello, " + name + "! (from snapshot-lib build 1)";
    }
}
