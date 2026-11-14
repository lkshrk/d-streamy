import SwiftUI

struct GuildChannelView: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Discord", systemImage: "bubble.left.and.bubble.right")
                .font(.subheadline.bold())

            FullWidthPicker(
                items: state.guilds,
                placeholder: state.isLoadingGuilds ? "Loading servers..." : "Select server...",
                title: { $0.name },
                selection: $state.selectedGuild
            )
            .onChange(of: state.selectedGuild) { _, guild in
                if let guild {
                    state.channels = []
                    Task { await state.fetchChannels(guildId: guild.id) }
                    state.selectedChannel = nil
                    state.persistConfig()
                }
            }

            FullWidthPicker(
                items: state.channels,
                placeholder: state.isLoadingChannels ? "Loading channels..." : "Select channel...",
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
