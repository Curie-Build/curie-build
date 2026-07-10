package com.example.isolated;

public final class Main {
    public static void main(String[] args) {
        String profile = System.getenv().getOrDefault("APP_PROFILE", "unset");
        System.out.println("isolated-app profile=" + profile);
    }
}
