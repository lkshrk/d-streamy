import SwiftUI

struct GuildChannelView: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Discord", systemImage: "bubble.left.and.bubble.right")
                .font(.subheadline.bold())

            FullWidthPicker(
                items: state.guilds,
                placeholder: "Select server...",
                title: { $0.name },
                selection: $state.selectedGuild
            )
            .onChange(of: state.selectedGuild) { _, guild in
                if let guild {
                    Task { await state.fetchChannels(guildId: guild.id) }
                    state.selectedChannel = nil
                    state.persistConfig()
                }
            }

            FullWidthPicker(
                items: state.channels,
                placeholder: "Select channel...",
                title: { $0.name },
                selection: $state.selectedChannel,
                isEnabled: state.selectedGuild != nil
            )
            .onChange(of: state.selectedChannel) { _, _ in
                state.persistConfig()
            }
        }
    }
}
