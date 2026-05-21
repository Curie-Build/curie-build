package com.example.springdemo.task

import com.example.springdemo.SpringBootDemoApplication
import org.springframework.beans.factory.annotation.Autowired
import org.springframework.boot.test.context.SpringBootTest
import org.testcontainers.containers.PostgreSQLContainer
import org.testcontainers.junit.jupiter.Container
import org.testcontainers.junit.jupiter.Testcontainers
import org.springframework.boot.testcontainers.service.connection.ServiceConnection
import spock.lang.Specification

/**
 * Integration test for {@link TaskRepository} using a real PostgreSQL database
 * managed by Testcontainers.
 *
 * {@code @ServiceConnection} (Spring Boot 3.1+) automatically wires the
 * container's JDBC URL, username and password into the datasource — no
 * manual property overrides needed.
 *
 * The schema ({@code schema.sql}) is applied by Spring Boot because
 * {@code spring.sql.init.mode=always} is set in {@code application.properties}.
 */
@SpringBootTest(classes = SpringBootDemoApplication,
                webEnvironment = SpringBootTest.WebEnvironment.NONE)
@Testcontainers
class TaskRepositorySpec extends Specification {

    @Container
    @ServiceConnection
    static PostgreSQLContainer<?> postgres =
            new PostgreSQLContainer<>("postgres:16-alpine")

    @Autowired
    TaskRepository repository

    def setup() {
        repository.deleteAll()
    }

    def "saves and retrieves a task with JSONB metadata"() {
        given: "a task with rich metadata"
        def task = Task.of("Buy milk", [priority: 2, tags: ["shopping", "food"]])

        when: "the task is persisted"
        def saved = repository.save(task)

        then: "the task has an auto-generated id"
        saved.id() != null

        and: "the task can be retrieved with all fields intact"
        with(repository.findById(saved.id()).orElseThrow()) {
            title() == "Buy milk"
            metadata()["priority"] == 2
            metadata()["tags"] == ["shopping", "food"]
        }
    }

    def "metadata defaults to empty map when not provided"() {
        when:
        def saved = repository.save(Task.of("Empty task", [:]))

        then:
        with(repository.findById(saved.id()).orElseThrow()) {
            title() == "Empty task"
            metadata() != null
            metadata().isEmpty()
        }
    }

    def "lists all persisted tasks"() {
        given:
        repository.save(Task.of("Task A", [x: 1]))
        repository.save(Task.of("Task B", [x: 2]))
        repository.save(Task.of("Task C", [x: 3]))

        expect:
        repository.findAll().size() == 3
    }

    def "returns empty optional for an unknown id"() {
        expect:
        repository.findById(Long.MAX_VALUE).isEmpty()
    }

    def "deletes a task by id"() {
        given:
        def saved = repository.save(Task.of("To delete", [:]))

        when:
        repository.deleteById(saved.id())

        then:
        repository.findById(saved.id()).isEmpty()
    }

    def "metadata JSONB roundtrips nested structures"() {
        given:
        def metadata = [
            nested: [key: "value", list: [1, 2, 3]],
            flag: true,
            score: 9.5
        ]

        when:
        def saved = repository.save(Task.of("Nested", metadata))
        def found = repository.findById(saved.id()).orElseThrow()

        then:
        found.metadata()["nested"]["key"] == "value"
        found.metadata()["nested"]["list"] == [1, 2, 3]
        found.metadata()["flag"] == true
    }
}
