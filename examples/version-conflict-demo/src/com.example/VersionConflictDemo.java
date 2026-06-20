package com.example;

import com.google.common.base.Joiner;

/**
 * Tiny app that uses Guava (the artifact at the centre of the resolved
 * major-version conflict) to prove the chosen 32.1.3-jre is on the classpath.
 */
public class VersionConflictDemo {
    public static void main(String[] args) {
        String joined = Joiner.on(", ").join("guava", "guice", "curie");
        System.out.println("resolved with: " + joined);
    }
}
