package com.example.layout;

import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.assertEquals;

class GreeterTest {
    @Test
    void greetsByName() {
        assertEquals("hello, Curie", Greeter.greet("Curie"));
    }
}
