package com.example.app;

import com.example.core.CoreUtils;
import com.example.greeter.Greeter;

/**
 * Application at the deepest nesting level (level 2) that depends on
 * libraries from level 1 ({@link Greeter}) and level 0 ({@link CoreUtils}).
 */
public class Main {
    public static void main(String[] args) {
        String name = args.length > 0 ? String.join(" ", args) : "Curie";

        Greeter greeter = new Greeter(name);
        System.out.println(greeter.greet());
        System.out.println(greeter.greetReversed());
        System.out.println("isBlank(\"\"): " + CoreUtils.isBlank(""));
        System.out.println("reverse(\"" + name + "\"): " + CoreUtils.reverse(name));
    }
}