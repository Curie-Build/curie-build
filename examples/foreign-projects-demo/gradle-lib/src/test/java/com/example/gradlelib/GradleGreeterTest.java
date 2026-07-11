package com.example.gradlelib;

import static org.junit.Assert.assertEquals;

import org.junit.Test;

public class GradleGreeterTest {
    @Test
    public void greetsByName() {
        assertEquals("Hello from Gradle, Curie!", GradleGreeter.greet("Curie"));
    }
}
