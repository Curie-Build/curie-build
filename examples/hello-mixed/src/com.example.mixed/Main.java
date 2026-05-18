package com.example.mixed;

public class Main {
    public static void main(String[] args) {
        Greeting g = new Greeting("Curie");
        System.out.println(g.message());
        System.out.println("Called from Java.");
    }
}
