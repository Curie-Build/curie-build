package com.example.stringutils;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

/**
 * Co-located unit tests for StringUtils (Maven layout, src/main/java/).
 * Confirms backward compatibility: *Test.java files in src/main/java/ are
 * treated as unit tests by Curie's test discovery.
 */
class StringUtilsTest {

    @Test
    void isBlank_null_returns_true() {
        assertTrue(StringUtils.isBlank(null));
    }

    @Test
    void isBlank_empty_returns_true() {
        assertTrue(StringUtils.isBlank(""));
    }

    @Test
    void isBlank_nonEmpty_returns_false() {
        assertFalse(StringUtils.isBlank("hello"));
    }

    @Test
    void capitalise_basic() {
        assertEquals("Hello", StringUtils.capitalise("hello"));
    }

    @Test
    void reverse_basic() {
        assertEquals("olleh", StringUtils.reverse("hello"));
    }

    @Test
    void countOccurrences_basic() {
        assertEquals(3, StringUtils.countOccurrences("banana", 'a'));
    }
}
