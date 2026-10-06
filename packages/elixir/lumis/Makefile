# Builds the `lumis` CLI into priv/native for Lumis.Port. elixir_make runs this
# from the package directory with MIX_APP_PATH set to the application's build
# directory. Cargo is run every time and decides itself whether anything changed.

REPOSITORY := $(abspath ../../..)
CARGO ?= cargo
TARGET_DIR := $(or $(CARGO_TARGET_DIR),$(REPOSITORY)/target)
PRIV_NATIVE := $(MIX_APP_PATH)/priv/native

all:
	$(CARGO) build --release --locked -p lumis-cli --manifest-path $(REPOSITORY)/Cargo.toml
	mkdir -p $(PRIV_NATIVE)
	cp $(TARGET_DIR)/release/lumis $(PRIV_NATIVE)/lumis

clean:
	rm -f $(PRIV_NATIVE)/lumis

.PHONY: all clean
