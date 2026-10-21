import Foundation

enum DaemonBinary {
    /// The bundled `Contents/MacOS/dstreamy`, else `DSTREAMY_BIN`, else `<repo>/rust/target/debug/dstreamy`.
    static func locate(
        bundleURL: URL = Bundle.main.bundleURL,
        environment: [String: String] = ProcessInfo.processInfo.environment,
        searchFrom: [URL] = defaultSearchRoots,
        fileManager: FileManager = .default
    ) -> URL? {
        let bundled = bundleURL.appendingPathComponent("Contents/MacOS/dstreamy")
        if fileManager.isExecutableFile(atPath: bundled.path) { return bundled }

        if let path = environment["DSTREAMY_BIN"], !path.isEmpty {
            return URL(fileURLWithPath: path)
        }

        for start in searchFrom {
            var dir = start.standardizedFileURL
            while dir.path != "/" {
                let candidate = dir.appendingPathComponent("rust/target/debug/dstreamy")
                if fileManager.isExecutableFile(atPath: candidate.path) { return candidate }
                dir.deleteLastPathComponent()
            }
        }
        return nil
    }

    static var defaultSearchRoots: [URL] {
        var roots: [URL] = []
        if let executable = Bundle.main.executableURL { roots.append(executable.deletingLastPathComponent()) }
        roots.append(URL(fileURLWithPath: FileManager.default.currentDirectoryPath))
        return roots
    }
}
