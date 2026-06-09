package com.example;

import com.example.greeter.GreetRequest;
import com.example.greeter.GreetResponse;

public class Greeter {
    public static GreetResponse greet(String name) {
        String message = "Hello, " + name + "!";
        return GreetResponse.newBuilder().setMessage(message).build();
    }

    public static void main(String[] args) {
        String name = args.length > 0 ? args[0] : "World";
        GreetResponse response = greet(name);
        System.out.println(response.getMessage());
    }
}
