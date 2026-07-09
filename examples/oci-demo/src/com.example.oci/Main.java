package com.example.oci;

import com.google.common.base.Strings;

public final class Main {
    public static void main(String[] args) {
        String name = args.length > 0 ? args[0] : "world";
        String env = System.getenv().getOrDefault("DEMO_ENV", "unset");
        System.out.println("hello, " + Strings.nullToEmpty(name) + " (env=" + env + ")");
    }
}
