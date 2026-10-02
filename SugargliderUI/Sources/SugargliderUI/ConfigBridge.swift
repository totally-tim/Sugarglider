// Copyright The Sugarglider Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

import Foundation

// MARK: - Config Data Types

/// Preferences configuration matching the Rust PreferencesJson struct.
/// Field names use camelCase to match Swift conventions (Rust uses snake_case in JSON).
public struct PreferencesConfig: Codable {
    // General settings
    public var statusIconEnable: Bool
    public var animate: Bool
    public var focusFollowsMouse: Bool
    public var mouseFollowsFocus: Bool
    public var outerGap: Double
    public var innerGap: Double

    // Dragging behavior
    public var dragDropEnable: Bool
    public var dragDropLivePreview: Bool

    // Layout settings
    public var defaultLayoutKind: String

    // Experimental features
    public var contextsEnable: Bool
    /// Which screens a context switch changes: "global" or "per_screen".
    public var contextsScope: String

    // Window rules
    public var windowRules: [WindowRuleJson]

    // Hotkey bindings (read-only)
    public var hotkeys: [HotkeyBinding]

    public init(
        statusIconEnable: Bool = true,
        animate: Bool = true,
        focusFollowsMouse: Bool = false,
        mouseFollowsFocus: Bool = false,
        outerGap: Double = 0,
        innerGap: Double = 0,
        dragDropEnable: Bool = true,
        dragDropLivePreview: Bool = true,
        defaultLayoutKind: String = "tree",
        contextsEnable: Bool = false,
        contextsScope: String = "global",
        windowRules: [WindowRuleJson] = [],
        hotkeys: [HotkeyBinding] = []
    ) {
        self.statusIconEnable = statusIconEnable
        self.animate = animate
        self.focusFollowsMouse = focusFollowsMouse
        self.mouseFollowsFocus = mouseFollowsFocus
        self.outerGap = outerGap
        self.innerGap = innerGap
        self.dragDropEnable = dragDropEnable
        self.dragDropLivePreview = dragDropLivePreview
        self.defaultLayoutKind = defaultLayoutKind
        self.contextsEnable = contextsEnable
        self.contextsScope = contextsScope
        self.windowRules = windowRules
        self.hotkeys = hotkeys
    }
}

/// Hotkey binding from the configuration.
public struct HotkeyBinding: Codable, Identifiable, Equatable {
    /// Identifies the binding while the Preferences window is open. Two
    /// bindings can have the same command, key, and description.
    public var id = UUID()

    /// The formatted hotkey string (e.g., "⌥H")
    public var key: String

    /// The bound command, as Rust encodes it. Sent back unchanged.
    public var command: String

    /// Human-readable description of what the command does
    public var description: String

    /// Category for grouping in the UI
    public var category: String

    /// The default hotkey for this command (if any)
    public var defaultKey: String?

    /// Whether this hotkey differs from its default
    public var isModified: Bool {
        guard let defaultKey = defaultKey else { return false }
        return key != defaultKey
    }

    public init(key: String, command: String, description: String, category: String, defaultKey: String? = nil) {
        self.key = key
        self.command = command
        self.description = description
        self.category = category
        self.defaultKey = defaultKey
    }

    enum CodingKeys: String, CodingKey {
        case key
        case command
        case description
        case category
        case defaultKey
    }
}

/// Window rule matching the Rust WindowRuleJson struct.
public struct WindowRuleJson: Codable, Identifiable {
    public var id = UUID()
    public var appName: String?
    public var bundleId: String?
    public var behavior: String
    /// Conditions that the App Rules pane doesn't show. Stored and sent back
    /// unchanged, so a save keeps them.
    public var titleRegex: String?
    public var titleSubstring: String?
    public var axRole: String?
    public var axSubrole: String?

    public init(
        appName: String? = nil, bundleId: String? = nil, behavior: String = "tile",
        titleRegex: String? = nil, titleSubstring: String? = nil,
        axRole: String? = nil, axSubrole: String? = nil
    ) {
        self.appName = appName
        self.bundleId = bundleId
        self.behavior = behavior
        self.titleRegex = titleRegex
        self.titleSubstring = titleSubstring
        self.axRole = axRole
        self.axSubrole = axSubrole
    }

    enum CodingKeys: String, CodingKey {
        case appName
        case bundleId
        case behavior
        case titleRegex
        case titleSubstring
        case axRole
        case axSubrole
    }
}

// MARK: - Config Bridge

/// What the Preferences window needs from Rust.
@MainActor
protocol PreferencesBackend: AnyObject {
    /// Loads the configuration of the running window manager.
    func loadConfig() throws -> PreferencesConfig
    /// Applies the configuration to the running window manager.
    func updateConfig(_ config: PreferencesConfig) throws
    /// Saves the configuration to the config file.
    func saveConfigToFile(_ config: PreferencesConfig) throws
}

/// Error types for config operations.
public enum ConfigBridgeError: LocalizedError {
    case loadFailed(String)
    case updateFailed(String)
    case saveFailed(String)
    case encodingFailed(String)
    case decodingFailed(String)

    public var errorDescription: String? {
        switch self {
        case .loadFailed(let msg): return "Failed to load config: \(msg)"
        case .updateFailed(let msg): return "Failed to update config: \(msg)"
        // Rust's message says that saving failed.
        case .saveFailed(let msg): return msg
        case .encodingFailed(let msg): return "Failed to encode config: \(msg)"
        case .decodingFailed(let msg): return "Failed to decode config: \(msg)"
        }
    }
}

/// Bridge for communicating config changes with the Rust backend.
@MainActor
public final class ConfigBridge: PreferencesBackend {
    public static let shared = ConfigBridge()

    private init() {}

    /// Load the current configuration from the Rust backend.
    public func loadConfig() throws -> PreferencesConfig {
        guard let ptr = sugarglider_get_config() else {
            throw ConfigBridgeError.loadFailed("No config available from backend")
        }
        defer { sugarglider_free_string(ptr) }

        let jsonString = String(cString: ptr)

        guard let data = jsonString.data(using: .utf8) else {
            throw ConfigBridgeError.decodingFailed("Invalid UTF-8 in config JSON")
        }

        do {
            let decoder = JSONDecoder()
            return try decoder.decode(PreferencesConfig.self, from: data)
        } catch {
            throw ConfigBridgeError.decodingFailed(error.localizedDescription)
        }
    }

    /// Update the running window manager with new configuration.
    /// This takes effect immediately but does not persist to disk.
    public func updateConfig(_ config: PreferencesConfig) throws {
        let encoder = JSONEncoder()
        let data: Data
        do {
            data = try encoder.encode(config)
        } catch {
            throw ConfigBridgeError.encodingFailed(error.localizedDescription)
        }

        guard let jsonString = String(data: data, encoding: .utf8) else {
            throw ConfigBridgeError.encodingFailed("Failed to create UTF-8 string from JSON")
        }

        let errorPtr = jsonString.withCString { cStr in
            sugarglider_update_config(cStr)
        }

        if let errorPtr = errorPtr {
            defer { sugarglider_free_string(errorPtr) }
            let errorMsg = String(cString: errorPtr)
            throw ConfigBridgeError.updateFailed(errorMsg)
        }
    }

    /// Save configuration to the TOML config file.
    /// This persists the settings to disk.
    public func saveConfigToFile(_ config: PreferencesConfig) throws {
        let encoder = JSONEncoder()
        let data: Data
        do {
            data = try encoder.encode(config)
        } catch {
            throw ConfigBridgeError.encodingFailed(error.localizedDescription)
        }

        guard let jsonString = String(data: data, encoding: .utf8) else {
            throw ConfigBridgeError.encodingFailed("Failed to create UTF-8 string from JSON")
        }

        let errorPtr = jsonString.withCString { cStr in
            sugarglider_save_config_to_file(cStr)
        }

        if let errorPtr = errorPtr {
            defer { sugarglider_free_string(errorPtr) }
            let errorMsg = String(cString: errorPtr)
            throw ConfigBridgeError.saveFailed(errorMsg)
        }
    }

    /// Update config and save to file in one operation.
    public func updateAndSave(_ config: PreferencesConfig) throws {
        try updateConfig(config)
        try saveConfigToFile(config)
    }
}
