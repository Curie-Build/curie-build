package com.example.mixed;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.assertEquals;

class GreetingTest {

    @Test
    void message_includes_name() {
        Greeting g = new Greeting("Curie");
        assertEquals("Hello, Curie, from Kotlin!", g.message());
    }

    @Test
    void data_class_equality_holds_across_languages() {
        // Kotlin `data class` generates equals/hashCode based on `name`;
        // calling those from Java must produce the same result.
        assertEquals(new Greeting("x"), new Greeting("x"));
        assertEquals(new Greeting("x").hashCode(), new Greeting("x").hashCode());
    }
}
