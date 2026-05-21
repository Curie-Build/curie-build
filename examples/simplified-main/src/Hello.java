// Unnamed class — no `package`, no `class Hello { ... }` wrapper.
// The file stem becomes the class name: Hello.
// Requires --enable-preview on Java 21–22; standard on Java 23+.

void main() {
    System.out.println("Hello from simplified main!");
    System.out.println("Java " + System.getProperty("java.version"));
}
