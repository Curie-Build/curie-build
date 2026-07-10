# Foreign Make project — produces out/legacy-lib.jar for Curie dependents.
# GREETING_SUFFIX is injected by the workspace [workspace.foreign.legacy-lib] env.

SRC := src/com/example/legacy/LegacyMessage.java
OUT := out
CLASSES := $(OUT)/classes
JAR := $(OUT)/legacy-lib.jar

.PHONY: all test clean

all: $(JAR)

$(JAR): $(SRC)
	mkdir -p $(CLASSES)
	# Bake GREETING_SUFFIX into a resource so env plumbing is observable.
	mkdir -p $(CLASSES)/com/example/legacy
	printf '%s' "$${GREETING_SUFFIX}" > $(CLASSES)/com/example/legacy/suffix.txt
	javac -d $(CLASSES) $(SRC)
	jar cf $(JAR) -C $(CLASSES) .

test:
	@test -f $(JAR) || $(MAKE) all
	@echo "legacy-lib: ok"

clean:
	rm -rf $(OUT)
