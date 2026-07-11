cmake_minimum_required(VERSION 3.16)
project(cmake_lib C)

add_library(cmake_lib STATIC src/greeter.c)
target_include_directories(cmake_lib PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)

enable_testing()
add_executable(test_greeter tests/test_greeter.c)
target_link_libraries(test_greeter PRIVATE cmake_lib)
add_test(NAME greeter COMMAND test_greeter)
