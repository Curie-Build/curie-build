package com.example.selective;

import com.google.common.collect.ImmutableList;
import org.apache.commons.lang3.StringUtils;

public final class Main {
    public static void main(String[] args) {
        ImmutableList<String> words = ImmutableList.of("curie", "maven", "sync");
        // Guava is shaded into the fat JAR; commons-lang3 is expected on the
        // runtime classpath (not bundled by Curie).
        System.out.println(StringUtils.join(words, ", "));
    }
}
