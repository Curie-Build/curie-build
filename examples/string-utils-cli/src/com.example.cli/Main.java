package com.example.cli;

import com.example.stringutils.StringUtils;

/**
 * Thin CLI demonstrating intra-workspace dependencies in Curie.
 *
 * Pulls in `com.example.stringutils.StringUtils` from the sibling
 * `string-utils` workspace member (declared under
 * [workspace-dependencies] in this module's Curie.toml).
 */
public class Main {
    public static void main(String[] args) {
        if (args.length == 0) {
            System.out.println("usage: string-utils-cli <text>");
            return;
        }
        String input = String.join(" ", args);

        System.out.println("input:        \"" + input + "\"");
        System.out.println("isBlank:      " + StringUtils.isBlank(input));
        System.out.println("capitalised:  \"" + StringUtils.capitalise(input) + "\"");
        System.out.println("reversed:     \"" + StringUtils.reverse(input) + "\"");
        System.out.println("space count:  " + StringUtils.countOccurrences(input, ' '));
    }
}
