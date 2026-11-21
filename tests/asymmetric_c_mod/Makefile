UNAME_S := $(shell uname -s)

ifeq ($(UNAME_S),Darwin)
    TARGET = libasymmetric_c_mod.dylib
    LDFLAGS = -shared -fPIC
else ifeq ($(OS),Windows_NT)
    TARGET = asymmetric_c_mod.dll
    LDFLAGS = -shared
else
    TARGET = libasymmetric_c_mod.so
    LDFLAGS = -shared -fPIC
endif

SIBLING_WIRE_INC := ../../../bamabi-wire/include
OVERLAY_CFG := ../../../.cargo/config.toml
OVERLAY_WIRE_ACTIVE := $(if $(wildcard $(OVERLAY_CFG)),$(shell sed 's/\#.*//' "$(OVERLAY_CFG)" 2>/dev/null | grep -E 'path[[:space:]]*=.*bamabi-wire'),)
USE_SIBLING_WIRE := $(filter 1 true TRUE yes YES,$(strip $(BAM_USE_SIBLING_WIRE)))$(OVERLAY_WIRE_ACTIVE)

ifneq ($(strip $(BAM_INCLUDE_DIR)),)
    INC_DIR := $(BAM_INCLUDE_DIR)
else ifneq ($(strip $(BAM_WIRE_DIR)),)
    INC_DIR := $(BAM_WIRE_DIR)/include
else ifneq ($(strip $(USE_SIBLING_WIRE)),)
    ifneq ($(wildcard $(SIBLING_WIRE_INC)/bamabi.h),)
        INC_DIR := $(SIBLING_WIRE_INC)
    else
        INC_DIR := ../../target/include
    endif
else
    INC_DIR := ../../target/include
endif

# Mandatory C99 compile flags (override prevents environment CFLAGS from clobbering)
override CFLAGS += -std=c99 -Wall -Wextra -Werror -I$(INC_DIR) -O3

all: $(TARGET)

ifeq ($(INC_DIR),../../target/include)
$(INC_DIR)/bamabi.h:
	@echo "Staging $(INC_DIR)/bamabi.h via bamabi_sdk_gen (standalone checkout)..."
	@cargo test --manifest-path ../../Cargo.toml -p bamabi_host --test ffi test_c99_shared_library_mutates_rust_components_via_vtable

$(INC_DIR)/generated/testbed_components.h: include/generated/testbed_components.h
	@mkdir -p $(dir $@)
	@cp $< $@
endif

$(TARGET): mod.c $(INC_DIR)/bamabi.h $(INC_DIR)/generated/testbed_components.h
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ $<

clean:
	rm -f $(TARGET)

.PHONY: all clean
