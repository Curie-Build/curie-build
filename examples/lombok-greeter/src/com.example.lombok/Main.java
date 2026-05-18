package com.example.lombok;

public class Main {
    public static void main(String[] args) {
        Greeting g = Greeting.builder()
            .recipient("Curie")
            .message("hello from a Lombok-generated builder")
            .exclamatory(true)
            .build();
        // @Getter generated `getRecipient()` / `getMessage()` / `isExclamatory()`.
        System.out.println("recipient: " + g.getRecipient());
        System.out.println("message:   " + g.getMessage());
        System.out.println("loud:      " + g.isExclamatory());
        // @ToString generated `toString()`.
        System.out.println("toString:  " + g);
    }
}
