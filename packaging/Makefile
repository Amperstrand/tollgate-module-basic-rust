PKG_MAKEFILE_DIR:=$(dir $(abspath $(lastword $(MAKEFILE_LIST))))

include $(TOPDIR)/rules.mk

PKG_NAME:=tollgate-rs
TOLLGATE_DISPLAY_VERSION:=$(if $(strip $(PACKAGE_VERSION)),$(PACKAGE_VERSION),0.1.0)

ifeq ($(CONFIG_USE_APK),y)
PKG_VERSION:=$(shell sh "$(PKG_MAKEFILE_DIR)normalize-apk-version.sh" "$(TOLLGATE_DISPLAY_VERSION)" 2>/dev/null || echo "$(TOLLGATE_DISPLAY_VERSION)")
else
PKG_VERSION:=$(TOLLGATE_DISPLAY_VERSION)
endif

PKG_FLAGS:=overwrite

# The Rust binary is pre-built by CI and staged into $(PKG_MAKEFILE_DIR).
# The SDK only packages it — no compilation happens here.
PREBUILT_BIN:=$(PKG_MAKEFILE_DIR)tollgate

PKG_BUILD_DIR:=$(BUILD_DIR)/$(PKG_NAME)-$(PKG_VERSION)

PKG_MAINTAINER:=TollGate <tollgate@tollgate.me>
PKG_LICENSE:=MIT
PKG_LICENSE_FILES:=LICENSE

PKG_BUILD_PARALLEL:=1
PKG_USE_MIPS16:=0

include $(INCLUDE_DIR)/package.mk

define Package/$(PKG_NAME)
	SECTION:=net
	CATEGORY:=Network
	TITLE:=TollGate (Rust)
	DEPENDS:=+nodogsplash +luci +jq
	PROVIDES:=tollgate-wrt
	CONFLICTS:=tollgate-wrt
	REPLACES:=tollgate-wrt
endef

define Package/$(PKG_NAME)/description
	TollGate payment gateway for OpenWrt (Rust implementation).
	Powered by Cashu ecash and CDK (Cashu Dev Kit).
endef

define Package/$(PKG_NAME)/preinst
#!/bin/sh
if [ -f /etc/tollgate/install.json ]; then
	CURRENT_TIMESTAMP=$$(date +%s)
	if ! jq ".install_time = $$CURRENT_TIMESTAMP" /etc/tollgate/install.json > /tmp/install.json.tmp; then
		echo "Error: Failed to update install_time using jq" >&2
		exit 1
	fi
	if ! mv /tmp/install.json.tmp /etc/tollgate/install.json; then
		echo "Error: Failed to move temporary file" >&2
		exit 1
	fi
else
	mkdir -p /etc/tollgate
	echo "{\"install_time\": $$(date +%s)}" > /etc/tollgate/install.json
fi
exit 0
endef

define Package/$(PKG_NAME)/postinst
#!/bin/sh
echo "TollGate Rust post-installation..."

# Run uci-defaults
for script in /etc/uci-defaults/99-tollgate-setup; do
    if [ -x "$$script" ]; then
        echo "Running $$script ..."
        "$$script" || echo "Warning: $$script exited with code $$?"
    fi
done

# Restart services
/etc/init.d/network restart 2>/dev/null || true
/etc/init.d/firewall reload 2>/dev/null || true
/etc/init.d/dnsmasq restart 2>/dev/null || true
/etc/init.d/uhttpd restart 2>/dev/null || true
/etc/init.d/nodogsplash restart 2>/dev/null || true

# Enable and start TollGate
if [ -x /etc/init.d/tollgate ]; then
    /etc/init.d/tollgate enable 2>/dev/null || true
    /etc/init.d/tollgate start 2>/dev/null || true
fi

echo "TollGate Rust installed successfully"
exit 0
endef

define Build/Prepare
	mkdir -p $(PKG_BUILD_DIR)
	[ -f "$(PREBUILT_BIN)" ] || { echo "ERROR: prebuilt binary not found at $(PREBUILT_BIN). CI should stage it before invoking the SDK." >&2; exit 1; }
	$(CP) $(PREBUILT_BIN) $(PKG_BUILD_DIR)/tollgate
	[ -f "$(PKG_MAKEFILE_DIR)LICENSE" ] && $(CP) $(PKG_MAKEFILE_DIR)LICENSE $(PKG_BUILD_DIR)/LICENSE || true
endef

define Build/Configure
endef

define Build/Compile
endef

define Package/$(PKG_NAME)/install
	$(INSTALL_DIR) $(1)/usr/bin
	$(INSTALL_BIN) $(PKG_BUILD_DIR)/tollgate $(1)/usr/bin/tollgate

	$(INSTALL_DIR) $(1)/etc/init.d
	$(INSTALL_BIN) $(PKG_MAKEFILE_DIR)files/etc/init.d/tollgate $(1)/etc/init.d/tollgate

	$(INSTALL_DIR) $(1)/etc/uci-defaults
	$(INSTALL_BIN) $(PKG_MAKEFILE_DIR)files/etc/uci-defaults/99-tollgate-setup $(1)/etc/uci-defaults/99-tollgate-setup

	$(INSTALL_DIR) $(1)/lib/upgrade/keep.d
	$(INSTALL_DATA) $(PKG_MAKEFILE_DIR)files/lib/upgrade/keep.d/tollgate $(1)/lib/upgrade/keep.d/tollgate

	$(INSTALL_DIR) $(1)/etc/tollgate
endef

$(eval $(call BuildPackage,tollgate-rs))
