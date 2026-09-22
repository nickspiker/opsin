// Make Opsin the default application for every content type its Info.plist claims.
//
// Registering a bundle with lsregister only makes it ELIGIBLE to open a type; becoming the DEFAULT is
// a separate per-type call, and stock macOS ships no command-line tool for it (`duti` is third-party).
// So: read the types out of the bundle's own Info.plist — one source of truth, no list to keep in sync —
// and claim each one.
//
//   swift set-default-handler.swift <path to Info.plist>
//
// A type the running macOS has never heard of fails with paramErr (-50) and is reported, not fatal:
// the UTI list covers cameras this machine may have no raw support for.

import CoreServices
import Foundation

let args = CommandLine.arguments
guard args.count == 2, let data = FileManager.default.contents(atPath: args[1]) else {
    FileHandle.standardError.write("usage: swift set-default-handler.swift <Info.plist>\n".data(using: .utf8)!)
    exit(2)
}

guard
    let plist = try? PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any],
    let bundleID = plist["CFBundleIdentifier"] as? String,
    let docTypes = plist["CFBundleDocumentTypes"] as? [[String: Any]]
else {
    FileHandle.standardError.write("\(args[1]): no CFBundleIdentifier / CFBundleDocumentTypes\n".data(using: .utf8)!)
    exit(1)
}

// Flattened in declaration order, de-duplicated — a type named twice is claimed once.
var types: [String] = []
for entry in docTypes {
    for uti in (entry["LSItemContentTypes"] as? [String]) ?? [] where !types.contains(uti) {
        types.append(uti)
    }
}

var claimed = 0
var unknown: [String] = []
for uti in types {
    let status = LSSetDefaultRoleHandlerForContentType(uti as CFString, .all, bundleID as CFString)
    if status == noErr { claimed += 1 } else { unknown.append("\(uti) (OSStatus \(status))") }
}

print("    \(claimed)/\(types.count) content types now open in \(bundleID)")
for u in unknown { print("    not known to this macOS, skipped: \(u)") }
