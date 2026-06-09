package com.example;

import com.example.model.GreetRequest;
import com.example.model.GreetResponse;

public class GreeterApp {
    public static void main(String[] args) {
        String name = args.length > 0 ? args[0] : "World";
        GreetRequest req = new GreetRequest().name(name);
        GreetResponse resp = new GreetResponse().message("Hello, " + req.getName() + "!");
        System.out.println(resp.getMessage());
    }
}
