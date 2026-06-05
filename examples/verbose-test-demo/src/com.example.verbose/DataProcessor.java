package com.example.verbose;

import java.util.List;
import java.util.stream.IntStream;

/**
 * Minimal library class for the verbose-test-demo example.
 * The real purpose of this module is to demonstrate curie inspect
 * with a large test log (~20 000 lines).
 */
public final class DataProcessor {

    private DataProcessor() {}

    /** Returns a list of {@code count} strings formatted as {@code prefix-N}. */
    public static List<String> generate(String prefix, int count) {
        return IntStream.rangeClosed(1, count)
                .mapToObj(i -> prefix + "-" + i)
                .toList();
    }

    /** Returns the sum of all values in the list. */
    public static int sum(List<Integer> values) {
        return values.stream().mapToInt(Integer::intValue).sum();
    }
}
