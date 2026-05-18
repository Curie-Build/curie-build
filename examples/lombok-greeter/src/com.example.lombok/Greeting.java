package com.example.lombok;

import lombok.Builder;
import lombok.Getter;
import lombok.ToString;

@Builder
@Getter
@ToString
public class Greeting {
    private final String recipient;
    private final String message;
    private final boolean exclamatory;
}
