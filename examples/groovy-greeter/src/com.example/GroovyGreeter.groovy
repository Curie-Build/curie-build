package com.example

class GroovyGreeter {
    static void main(String[] args) {
        def name = args.length > 0 ? args[0] : "World"
        println "Hello from Groovy, ${name}!"
    }
}
