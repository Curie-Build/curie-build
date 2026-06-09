package com.example.springdemo.task

import com.example.springdemo.SpringBootDemoApplication
import org.springframework.beans.factory.annotation.Autowired
import org.springframework.boot.test.context.SpringBootTest
import spock.lang.Specification

/**
 * Integration test for {@link TaskStore} using the full Spring Boot context.
 * No database or Docker daemon required — the store is purely in-memory.
 */
@SpringBootTest(classes = SpringBootDemoApplication,
                webEnvironment = SpringBootTest.WebEnvironment.NONE)
class TaskStoreSpec extends Specification {

    @Autowired
    TaskStore store

    def setup() {
        store.deleteAll()
    }

    def "saves and retrieves a task"() {
        given:
        def task = Task.of("Buy milk", [priority: 2, tags: ["shopping", "food"]])

        when:
        def saved = store.save(task)

        then:
        saved.id() != null

        and:
        with(store.findById(saved.id()).orElseThrow()) {
            title() == "Buy milk"
            metadata()["priority"] == 2
            metadata()["tags"] == ["shopping", "food"]
        }
    }

    def "metadata defaults to empty map"() {
        when:
        def saved = store.save(Task.of("Empty task", [:]))

        then:
        with(store.findById(saved.id()).orElseThrow()) {
            title() == "Empty task"
            metadata().isEmpty()
        }
    }

    def "lists all persisted tasks"() {
        given:
        store.save(Task.of("Task A", [x: 1]))
        store.save(Task.of("Task B", [x: 2]))
        store.save(Task.of("Task C", [x: 3]))

        expect:
        store.findAll().size() == 3
    }

    def "returns empty optional for an unknown id"() {
        expect:
        store.findById(Long.MAX_VALUE).isEmpty()
    }

    def "deletes a task by id"() {
        given:
        def saved = store.save(Task.of("To delete", [:]))

        when:
        store.deleteById(saved.id())

        then:
        store.findById(saved.id()).isEmpty()
    }
}
