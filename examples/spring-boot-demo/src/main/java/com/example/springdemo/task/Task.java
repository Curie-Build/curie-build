package com.example.springdemo.task;

import java.util.Map;

public record Task(Long id, String title, Map<String, Object> metadata) {

    /** Factory for unsaved tasks — the store assigns the id on save. */
    public static Task of(String title, Map<String, Object> metadata) {
        return new Task(null, title, metadata);
    }
}
