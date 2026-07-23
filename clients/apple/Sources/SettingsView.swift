import SwiftUI

/// A selectable language (id == the server's ISO 639-2/B code).
private struct Lang: Identifiable {
    let id: String
    let name: String
}

private let languages: [Lang] = [
    Lang(id: "eng", name: "English"), Lang(id: "jpn", name: "Japanese"),
    Lang(id: "spa", name: "Spanish"), Lang(id: "fre", name: "French"),
    Lang(id: "ger", name: "German"), Lang(id: "ita", name: "Italian"),
    Lang(id: "por", name: "Portuguese"), Lang(id: "kor", name: "Korean"),
    Lang(id: "chi", name: "Chinese"), Lang(id: "rus", name: "Russian"),
    Lang(id: "hin", name: "Hindi"), Lang(id: "ara", name: "Arabic"),
]
private let subtitleLanguages: [Lang] = [Lang(id: "off", name: "Off")] + languages

struct SettingsView: View {
    @EnvironmentObject var model: AppModel

    private var audioBinding: Binding<String> {
        Binding(get: { model.audioLang },
                set: { model.setLanguages(audio: $0, sub: model.subLang) })
    }
    private var subBinding: Binding<String> {
        Binding(get: { model.subLang },
                set: { model.setLanguages(audio: model.audioLang, sub: $0) })
    }

    var body: some View {
        Form {
            Section {
                Picker("Audio language", selection: audioBinding) {
                    ForEach(languages) { Text($0.name).tag($0.id) }
                }
                Picker("Subtitle language", selection: subBinding) {
                    ForEach(subtitleLanguages) { Text($0.name).tag($0.id) }
                }
            } header: {
                Text("Playback defaults")
            } footer: {
                Text("Preferred tracks when a title has more than one. Applied on the fly for direct play.")
            }

            Section("Account") {
                LabeledContent("Signed in as", value: model.username ?? "—")
                LabeledContent("Server", value: model.serverName ?? model.origin)
                Button("Sign out", role: .destructive) { model.logout() }
                Button("Change server") { model.changeServer() }
            }
        }
        .navigationTitle("Settings")
        #if os(iOS)
        .navigationBarTitleDisplayMode(.inline)
        #endif
    }
}
