package com.example.mixed

data class Greeting(val name: String) {
    fun message(): String = "Hello, $name, from Kotlin!"
}
