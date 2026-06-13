package com.example.testap;

import com.google.auto.value.AutoValue;

/**
 * A test-only value type.  AutoValue generates {@code AutoValue_Money} at
 * test-compile time — which only works if the AutoValue processor runs during
 * test compilation.
 */
@AutoValue
public abstract class Money {
    public abstract int cents();

    public static Money of(int cents) {
        return new AutoValue_Money(cents);
    }
}
