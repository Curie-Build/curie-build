package com.example

import spock.lang.Specification
import spock.lang.Unroll

class CalculatorSpec extends Specification {

    def calc = new Calculator()

    def "add returns the sum of two numbers"() {
        expect:
        calc.add(3, 4) == 7
    }

    @Unroll
    def "multiply #a * #b == #result"() {
        expect:
        calc.multiply(a, b) == result

        where:
        a | b | result
        2 | 3 |  6
        0 | 5 |  0
        4 | 4 | 16
    }

    def "subtract gives the right difference"() {
        expect:
        calc.subtract(10, 3) == 7
    }
}
