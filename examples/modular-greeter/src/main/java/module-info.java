module com.example.greeter {
    requires com.fasterxml.jackson.databind;
    requires kotlin.stdlib;
    // Jackson uses deep reflection to serialise Greeting; opens grants that access.
    opens com.example.greeter to com.fasterxml.jackson.databind;
}
