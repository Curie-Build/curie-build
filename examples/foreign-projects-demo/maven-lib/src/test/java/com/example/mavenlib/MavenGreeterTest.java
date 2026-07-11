package com.example.mavenlib;

import static org.junit.Assert.assertEquals;

import org.junit.Test;

public class MavenGreeterTest {
    @Test
    public void greetsByName() {
        assertEquals("Hello from Maven, Curie!", MavenGreeter.greet("Curie"));
    }
}
