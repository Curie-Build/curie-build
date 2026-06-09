package com.example.springdemo.task;

import org.springframework.stereotype.Component;

import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicLong;

/** Thread-safe in-memory store — no database required. */
@Component
public class TaskStore {

    private final Map<Long, Task> store = new ConcurrentHashMap<>();
    private final AtomicLong idSeq = new AtomicLong(1);

    public List<Task> findAll() {
        return List.copyOf(store.values());
    }

    public Optional<Task> findById(Long id) {
        return Optional.ofNullable(store.get(id));
    }

    public Task save(Task task) {
        Long id = task.id() != null ? task.id() : idSeq.getAndIncrement();
        Task saved = new Task(id, task.title(), task.metadata());
        store.put(id, saved);
        return saved;
    }

    public void deleteById(Long id) {
        store.remove(id);
    }

    public void deleteAll() {
        store.clear();
    }
}
