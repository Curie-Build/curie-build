package com.example.greeter;

import com.example.core.CoreUtils;

/**
 * A greeter that uses {@link CoreUtils} from the root-level core-lib.
 * Demonstrates cross-level workspace dependencies: this library lives
 * inside the services/ nested workspace, but depends on core-lib from
 * the root workspace.
 */
public final class Greeter {

    private final String name;

    public Greeter(String name) {
        this.name = CoreUtils.isBlank(name) ? "World" : name;
    }

    public String greet() {
        return "Hello, " + name + "!";
    }

    public String greetReversed() {
        return "Hello, " + CoreUtils.reverse(name) + "!";
    }
}