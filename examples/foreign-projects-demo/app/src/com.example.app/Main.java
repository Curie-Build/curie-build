package com.example.app;

import com.example.gradlelib.GradleGreeter;
import com.example.greeter.Greeter;
import com.example.legacy.LegacyMessage;
import com.example.mavenlib.MavenGreeter;

public final class Main {
    public static void main(String[] args) {
        String name = args.length > 0 ? args[0] : "world";
        System.out.println(Greeter.greet(name));
        System.out.println(LegacyMessage.text());
        System.out.println(MavenGreeter.greet(name));
        System.out.println(GradleGreeter.greet(name));
    }
}
