package com.example.testap;

import static org.junit.jupiter.api.Assertions.assertEquals;

import org.junit.jupiter.api.Test;

class MoneyTest {
    @Test
    void generatesValueClass() {
        Money m = Money.of(150);
        assertEquals(150, m.cents());
        // AutoValue-generated equals() — proves the processor ran.
        assertEquals(Money.of(150), m);
    }
}
