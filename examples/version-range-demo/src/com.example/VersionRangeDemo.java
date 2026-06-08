package com.example;

/**
 * Demonstrates that system-rules and its pinned transitive dep are resolved
 * and compiled successfully.
 *
 * system-rules provides JUnit 4 rules for capturing and restoring System
 * state (stdout, stderr, environment variables, etc.) in tests.  Its POM
 * declares junit:junit-dep with the version range [4.9,), which Curie
 * rejects unless the user pins an exact version in Curie.toml.
 */
public class VersionRangeDemo {
    public static void main(String[] args) {
        System.out.println("system-rules : " +
            org.junit.contrib.java.lang.system.SystemOutRule.class.getName());
        System.out.println("junit-dep    : " +
            org.junit.rules.ExternalResource.class.getName());
        System.out.println("Both JARs resolved with exact, pinned versions.");
    }
}
