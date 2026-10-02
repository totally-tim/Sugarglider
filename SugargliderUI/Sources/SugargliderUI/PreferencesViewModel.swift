// Copyright The Sugarglider Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

import SwiftUI
import Combine
import ServiceManagement

// MARK: - Types

public enum LayoutMode: String, CaseIterable {
    case tree = "tree"
    case column = "column"
}

public enum SplitDirection: String, CaseIterable {
    case horizontal = "horizontal"
    case vertical = "vertical"
    case auto = "auto"
}

public enum AppBehavior: String, CaseIterable {
    case tile = "Tile"
    case float = "Float"
    case ignore = "Ignore"
}

public struct AppRule: Identifiable {
    public let id = UUID()
    /// The app name condition. `nil` means the rule has none, which is
    /// different from an empty string, which the file may spell.
    public var appName: String?
    /// The bundle id condition, as `appName`.
    public var bundleId: String?
    public var behavior: AppBehavior
    /// Window-rule conditions the App Rules pane doesn't show. Stored and
    /// saved unchanged.
    public var titleRegex: String?
    public var titleSubstring: String?
    public var axRole: String?
    public var axSubrole: String?

    public init(
        appName: String?, bundleId: String?, behavior: AppBehavior,
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
}

// MARK: - View Model

@MainActor
public class PreferencesViewModel: ObservableObject {
    // General
    @Published public var launchAtLogin: Bool = false
    @Published public var showMenuBarIcon: Bool = true
    @Published public var enableAnimations: Bool = true
    @Published public var focusFollowsMouse: Bool = false
    @Published public var mouseFollowsFocus: Bool = false
    @Published public var outerGap: Double = 0
    @Published public var innerGap: Double = 0

    // Dragging behavior
    @Published public var dragDropEnable: Bool = true
    @Published public var dragDropLivePreview: Bool = true

    // Layout
    @Published public var defaultLayout: LayoutMode = .tree
    @Published public var defaultSplitDirection: SplitDirection = .auto
    @Published public var defaultColumnWidth: Double = 400

    // Experimental
    @Published public var contextsEnable: Bool = false
    /// Which screens a context switch changes: "global" or "per_screen".
    @Published public var contextsScope: String = "global"

    // Hotkeys (read-only, loaded from config)
    @Published public var hotkeys: [HotkeyBinding] = []

    /// Hotkeys grouped by category for display. A category missing from the
    /// order follows the others, so that no binding is hidden.
    public var hotkeysByCategory: [(category: String, bindings: [HotkeyBinding])] {
        let grouped = Dictionary(grouping: hotkeys) { $0.category }
        // Define category order
        let categoryOrder = [
            "System", "Focus", "Move", "Resize", "Layout", "Floating", "Scroll Layout", "Contexts",
            "Developer", "Utilities",
        ]
        let otherCategories = grouped.keys.filter { !categoryOrder.contains($0) }.sorted()
        return (categoryOrder + otherCategories).compactMap { category in
            guard let bindings = grouped[category], !bindings.isEmpty else { return nil }
            return (category: category, bindings: bindings)
        }
    }

    // App Rules
    @Published public var appRules: [AppRule] = []

    // Error state for UI feedback
    @Published public var lastError: String? = nil

    private let backend: PreferencesBackend
    private var cancellables = Set<AnyCancellable>()
    private var isLoading = false
    /// Set when the initial load failed. Saving stays off until a load
    /// succeeds, so the defaults the window shows can't replace the running
    /// config.
    private var loadFailed = false

    public convenience init() {
        self.init(backend: ConfigBridge.shared)
    }

    init(backend: PreferencesBackend) {
        self.backend = backend
        loadFromConfig()
        setupAutoSave()
        loadLaunchAtLogin()
    }

    public func addAppRule() {
        appRules.append(AppRule(appName: "New App", bundleId: nil, behavior: .tile))
    }

    /// Update a hotkey binding and save to config
    public func updateHotkey(id: HotkeyBinding.ID, newKey: String) {
        guard let index = hotkeys.firstIndex(where: { $0.id == id }) else {
            return
        }
        if let other = rowUsing(newKey, besides: id) {
            lastError = "\(newKey) is already assigned to \"\(other.description)\". The key was not changed."
            return
        }

        // Update the local state
        hotkeys[index].key = newKey

        // Save to config
        saveToConfig()
    }

    /// Reset a hotkey to its default value
    public func resetHotkeyToDefault(id: HotkeyBinding.ID) {
        guard let index = hotkeys.firstIndex(where: { $0.id == id }),
              let defaultKey = hotkeys[index].defaultKey else {
            return
        }
        if let other = rowUsing(defaultKey, besides: id) {
            lastError = "\(defaultKey) is already assigned to \"\(other.description)\". The key was not changed."
            return
        }

        // Update the local state to the default
        hotkeys[index].key = defaultKey

        // Save to config
        saveToConfig()
    }

    /// The other row that already uses `key`, if any. The window refuses a
    /// key that another row uses, because two rows with one hotkey abort the
    /// window manager when it registers them.
    private func rowUsing(_ key: String, besides id: HotkeyBinding.ID) -> HotkeyBinding? {
        hotkeys.first { $0.id != id && $0.key == key }
    }

    // MARK: - Config Loading

    private func loadFromConfig() {
        isLoading = true
        defer { isLoading = false }

        do {
            let config = try backend.loadConfig()
            loadFailed = false
            applyConfig(config)
        } catch {
            loadFailed = true
            lastError = error.localizedDescription
            print("Failed to load config: \(error)")
        }
    }

    private func applyConfig(_ config: PreferencesConfig) {
        showMenuBarIcon = config.statusIconEnable
        enableAnimations = config.animate
        focusFollowsMouse = config.focusFollowsMouse
        mouseFollowsFocus = config.mouseFollowsFocus
        outerGap = config.outerGap
        innerGap = config.innerGap
        dragDropEnable = config.dragDropEnable
        dragDropLivePreview = config.dragDropLivePreview

        // Map layout kind: "scroll" -> .column, "tree" -> .tree
        defaultLayout = config.defaultLayoutKind == "scroll" ? .column : .tree

        contextsEnable = config.contextsEnable
        contextsScope = config.contextsScope

        // Load hotkeys
        hotkeys = config.hotkeys

        // Map window rules
        appRules = config.windowRules.map { rule in
            let behavior: AppBehavior
            switch rule.behavior.lowercased() {
            case "float": behavior = .float
            case "ignore": behavior = .ignore
            default: behavior = .tile
            }
            return AppRule(
                appName: rule.appName,
                bundleId: rule.bundleId,
                behavior: behavior,
                titleRegex: rule.titleRegex,
                titleSubstring: rule.titleSubstring,
                axRole: rule.axRole,
                axSubrole: rule.axSubrole
            )
        }
    }

    // MARK: - Auto-Save Setup

    private func setupAutoSave() {
        // Create publishers for all saveable properties
        let publishers: [AnyPublisher<Void, Never>] = [
            $showMenuBarIcon.map { _ in () }.eraseToAnyPublisher(),
            $enableAnimations.map { _ in () }.eraseToAnyPublisher(),
            $focusFollowsMouse.map { _ in () }.eraseToAnyPublisher(),
            $mouseFollowsFocus.map { _ in () }.eraseToAnyPublisher(),
            $outerGap.map { _ in () }.eraseToAnyPublisher(),
            $innerGap.map { _ in () }.eraseToAnyPublisher(),
            $dragDropEnable.map { _ in () }.eraseToAnyPublisher(),
            $dragDropLivePreview.map { _ in () }.eraseToAnyPublisher(),
            $defaultLayout.map { _ in () }.eraseToAnyPublisher(),
            $contextsEnable.map { _ in () }.eraseToAnyPublisher(),
            $contextsScope.map { _ in () }.eraseToAnyPublisher(),
            $appRules.map { _ in () }.eraseToAnyPublisher(),
        ]

        Publishers.MergeMany(publishers)
            .dropFirst(publishers.count) // Skip initial values
            .debounce(for: .milliseconds(300), scheduler: RunLoop.main)
            .sink { [weak self] in
                guard let self = self, !self.isLoading else { return }
                self.saveToConfig()
            }
            .store(in: &cancellables)
    }

    // MARK: - Config Saving

    public func saveToConfig() {
        guard !loadFailed else {
            // The window's values are defaults and its key list is empty,
            // because the initial load failed. Saving either would replace
            // the running config and the file with them.
            return
        }
        let config = buildConfig()

        do {
            // Write the file first. The running app changes only after the
            // write succeeds, so a failed save can't leave the two apart.
            try backend.saveConfigToFile(config)
            try backend.updateConfig(config)
            lastError = nil
        } catch {
            lastError = error.localizedDescription
            print("Failed to save config: \(error)")
        }
    }

    private func buildConfig() -> PreferencesConfig {
        PreferencesConfig(
            statusIconEnable: showMenuBarIcon,
            animate: enableAnimations,
            focusFollowsMouse: focusFollowsMouse,
            mouseFollowsFocus: mouseFollowsFocus,
            outerGap: outerGap,
            innerGap: innerGap,
            dragDropEnable: dragDropEnable,
            dragDropLivePreview: dragDropLivePreview,
            // Map layout mode: .column -> "scroll", .tree -> "tree"
            defaultLayoutKind: defaultLayout == .column ? "scroll" : "tree",
            contextsEnable: contextsEnable,
            contextsScope: contextsScope,
            windowRules: appRules.map { rule in
                WindowRuleJson(
                    appName: rule.appName,
                    bundleId: rule.bundleId,
                    behavior: rule.behavior.rawValue.lowercased(),
                    titleRegex: rule.titleRegex,
                    titleSubstring: rule.titleSubstring,
                    axRole: rule.axRole,
                    axSubrole: rule.axSubrole
                )
            },
            hotkeys: hotkeys
        )
    }

    // MARK: - Launch at Login

    private func loadLaunchAtLogin() {
        if #available(macOS 13.0, *) {
            launchAtLogin = SMAppService.mainApp.status == .enabled
        }
    }

    public func setLaunchAtLogin(_ enabled: Bool) {
        guard #available(macOS 13.0, *) else { return }

        do {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            launchAtLogin = enabled
        } catch {
            print("Failed to set launch at login: \(error)")
            // Revert UI state
            launchAtLogin = !enabled
        }
    }
}
