#include "greeter.h"

#include <stdio.h>
#include <string.h>

int main(void) {
    const char *msg = cmake_greet();
    if (strcmp(msg, "hello from cmake") != 0) {
        fprintf(stderr, "unexpected greeting: %s\n", msg);
        return 1;
    }
    printf("cmake-lib: ok\n");
    return 0;
}
