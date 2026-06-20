package com.example.stringutils;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

/**
 * Unit tests for StringUtils (Maven layout, src/test/java/).
 * Confirms backward compatibility: the classic Maven layout — production in
 * src/main/java/, tests in src/test/java/ — works unchanged.  (In Maven
 * layout a *Test class under src/main/java/ is production code, unlike the
 * flat-package layout's co-located-test convention.)
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
