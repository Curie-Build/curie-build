package com.example.stringutils;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

/**
 * Edge-case integration tests for StringUtils (Maven layout, src/test/java/).
 * Confirms backward compatibility: files in src/test/java/ are treated as
 * integration tests by Curie's test discovery.
 */
class StringUtilsEdgeCaseTest {

    @Test
    void capitalise_single_char() {
        assertEquals("A", StringUtils.capitalise("a"));
    }

    @Test
    void capitalise_blank_returns_blank() {
        assertEquals("   ", StringUtils.capitalise("   "));
    }

    @Test
    void reverse_empty_string() {
        assertEquals("", StringUtils.reverse(""));
    }

    @Test
    void reverse_palindrome() {
        assertEquals("racecar", StringUtils.reverse("racecar"));
    }

    @Test
    void countOccurrences_null_returns_zero() {
        assertEquals(0, StringUtils.countOccurrences(null, 'a'));
    }

    @Test
    void countOccurrences_entire_string() {
        assertEquals(5, StringUtils.countOccurrences("aaaaa", 'a'));
    }
}
