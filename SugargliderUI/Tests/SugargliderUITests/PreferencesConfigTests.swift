// Copyright The Sugarglider Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

import Foundation
import XCTest

@testable import SugargliderUI

/// The JSON that `ConfigBridge` sends to Rust. The Rust test
/// `preferences_from_the_swift_ui_save_with_their_key_bindings` in
/// `src/config.rs` saves the same fixture.
final class PreferencesConfigTests: XCTestCase {
  private static let fixtureURL = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()  // SugargliderUITests
    .deletingLastPathComponent()  // Tests
    .deletingLastPathComponent()  // SugargliderUI
    .deletingLastPathComponent()  // repository root
    .appendingPathComponent("tests/fixtures/preferences-from-swift.json")

  private static let fixtureConfig = PreferencesConfig(
    statusIconEnable: true,
    animate: false,
    focusFollowsMouse: true,
    mouseFollowsFocus: false,
    outerGap: 8,
    innerGap: 4,
    dragDropEnable: true,
    dragDropLivePreview: false,
    defaultLayoutKind: "tree",
    contextsEnable: true,
    windowRules: [
      WindowRuleJson(appName: "Finder", bundleId: "com.apple.finder", behavior: "float"),
      WindowRuleJson(appName: "Calculator", behavior: "float"),
    ],
    hotkeys: [
      HotkeyBinding(
        key: "⌥Z", command: #""toggle_global_enabled""#, description: "Toggle tiling globally",
        category: "System", defaultKey: "⌥Z"),
      HotkeyBinding(
        key: "⌃⌥⇧H", command: #"{"move_focus":"left"}"#, description: "Focus left",
        category: "Focus", defaultKey: "⌃⌥⇧←"),
      HotkeyBinding(
        key: "⌥T", command: #"{"exec":"open -a Terminal"}"#, description: "Execute command",
        category: "Utilities"),
    ]
  )

  private static func jsonObject(_ data: Data) throws -> NSDictionary {
    let object = try JSONSerialization.jsonObject(with: data)
    return try XCTUnwrap(object as? NSDictionary)
  }

  func testEncodesTheFixtureThatRustSaves() throws {
    let encoded = try Self.jsonObject(JSONEncoder().encode(Self.fixtureConfig))
    let fixture = try Self.jsonObject(Data(contentsOf: Self.fixtureURL))

    XCTAssertEqual(fixture, encoded)
  }

  /// `sugarglider_get_config` sends the contexts switch and a sort order for
  /// each key binding.
  func testDecodesTheContextsSwitchThatRustSends() throws {
    let json = """
      {
        "statusIconEnable": true, "animate": true, "focusFollowsMouse": false,
        "mouseFollowsFocus": false, "outerGap": 0.0, "innerGap": 0.0,
        "dragDropEnable": true, "dragDropLivePreview": true, "defaultLayoutKind": "tree",
        "contextsEnable": true, "contextsScope": "global", "windowRules": [],
        "hotkeys": [
          { "key": "⌥Z", "command": "\\"toggle_global_enabled\\"",
            "description": "Toggle tiling globally", "category": "System",
            "defaultKey": "⌥Z", "sortOrder": 0 }
        ]
      }
      """

    let config = try JSONDecoder().decode(PreferencesConfig.self, from: Data(json.utf8))

    XCTAssertTrue(config.contextsEnable)
    XCTAssertEqual(config.hotkeys.map(\.command), [#""toggle_global_enabled""#])
    XCTAssertFalse(PreferencesConfig().contextsEnable)
  }

  /// A window rule's conditions decode and encode under the names Rust
  /// writes, so the window carries them back in a save.
  func testDecodesAndEncodesWindowRuleConditions() throws {
    let json = """
      {
        "appName": "Finder", "behavior": "float",
        "titleRegex": "Picture-in-Picture", "titleSubstring": "Prefs",
        "axRole": "AXWindow", "axSubrole": "AXDialog"
      }
      """
    let rule = try JSONDecoder().decode(WindowRuleJson.self, from: Data(json.utf8))

    XCTAssertEqual(rule.titleRegex, "Picture-in-Picture")
    XCTAssertEqual(rule.titleSubstring, "Prefs")
    XCTAssertEqual(rule.axRole, "AXWindow")
    XCTAssertEqual(rule.axSubrole, "AXDialog")

    let encoded = try Self.jsonObject(JSONEncoder().encode(rule))
    XCTAssertEqual(encoded["titleRegex"] as? String, "Picture-in-Picture")
    XCTAssertEqual(encoded["titleSubstring"] as? String, "Prefs")
    XCTAssertEqual(encoded["axRole"] as? String, "AXWindow")
    XCTAssertEqual(encoded["axSubrole"] as? String, "AXDialog")
  }

  /// The scope picker's value travels as `contextsScope`.
  func testDecodesAndEncodesTheContextsScope() throws {
    let json = """
      {
        "statusIconEnable": true, "animate": true, "focusFollowsMouse": false,
        "mouseFollowsFocus": false, "outerGap": 0.0, "innerGap": 0.0,
        "dragDropEnable": true, "dragDropLivePreview": true, "defaultLayoutKind": "tree",
        "contextsEnable": true, "contextsScope": "per_screen", "windowRules": [],
        "hotkeys": []
      }
      """
    let config = try JSONDecoder().decode(PreferencesConfig.self, from: Data(json.utf8))
    XCTAssertEqual(config.contextsScope, "per_screen")

    let encoded = try Self.jsonObject(JSONEncoder().encode(config))
    XCTAssertEqual(encoded["contextsScope"] as? String, "per_screen")
  }

  /// Each decoded binding is its own row, even when two are alike.
  func testDecodedBindingsHaveTheirOwnIdentity() throws {
    let binding = """
      { "key": "⌥Q", "command": "{\\"exec\\":\\"open -a Terminal\\"}",
        "description": "Execute command", "category": "Utilities" }
      """
    let hotkeys = try JSONDecoder().decode(
      [HotkeyBinding].self, from: Data("[\(binding), \(binding)]".utf8))

    XCTAssertEqual(hotkeys.count, 2)
    XCTAssertNotEqual(hotkeys[0].id, hotkeys[1].id)
  }

  /// The row identity does not derive from a command name. A context called
  /// "x#2" therefore cannot collide with a second binding for "x".
  func testBindingsWithFormerCommandIdCollisionRetainCommandsAndIdentity() throws {
    let commandX = #"{"switch_context":"x"}"#
    let commandXSuffixed = #"{"switch_context":"x#2"}"#
    let config = PreferencesConfig(hotkeys: [
      HotkeyBinding(key: "⌃⌥A", command: commandX, description: "Switch to x", category: "Contexts"),
      HotkeyBinding(key: "⌃⌥B", command: commandX, description: "Switch to x", category: "Contexts"),
      HotkeyBinding(
        key: "⌃⌥C", command: commandXSuffixed, description: "Switch to x#2", category: "Contexts"),
    ])

    let decoded = try JSONDecoder().decode(PreferencesConfig.self, from: JSONEncoder().encode(config))

    XCTAssertEqual(decoded.hotkeys.map(\.command), [commandX, commandX, commandXSuffixed])
    XCTAssertEqual(Set(decoded.hotkeys.map(\.id)).count, 3)
  }
}
