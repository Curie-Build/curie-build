package com.example.av;

import com.google.auto.value.AutoValue;

/**
 * Abstract value class.  AutoValue's annotation processor reads the
 * abstract accessors and generates a final {@code AutoValue_Animal}
 * subclass with equals/hashCode/toString implementations.
 *
 * The generated class lives under
 * {@code target/generated-sources/annotations/com/example/av/AutoValue_Animal.java}
 * and is compiled into {@code target/classes/com/example/av/AutoValue_Animal.class}
 * alongside this one — fully tracked by Curie's per-build manifest, so
 * removing the {@code @AutoValue} line cleans the generated class up
 * automatically on the next build.
 */
@AutoValue
public abstract class Animal {
    public abstract String name();
    public abstract int legs();

    public static Animal create(String name, int legs) {
        return new AutoValue_Animal(name, legs);
    }
}
