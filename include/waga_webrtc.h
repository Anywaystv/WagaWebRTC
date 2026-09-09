#ifndef WAGA_WEBRTC_H
#define WAGA_WEBRTC_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct WagaPeer WagaPeer;

typedef enum WagaCodec {
    WAGA_CODEC_NONE = 0,
    WAGA_CODEC_H264 = 1,
    WAGA_CODEC_H265 = 2,
    WAGA_CODEC_OPUS = 3,
    WAGA_CODEC_AAC = 4,
} WagaCodec;

typedef enum WagaEvent {
    WAGA_EVENT_NONE = 0,
    WAGA_EVENT_CONNECTED = 1,
    WAGA_EVENT_DISCONNECTED = 2,
    WAGA_EVENT_CLOSED = 3,
    WAGA_EVENT_KEYFRAME_REQUEST = 4,
} WagaEvent;

typedef struct WagaTransmit {
    const char *source;
    const char *destination;
    const uint8_t *data;
    size_t length;
} WagaTransmit;

typedef struct WagaMedia {
    int32_t codec;
    uint64_t media_time;
    uint32_t clock_rate;
    uint64_t ntp_micros;
    uint64_t sender_media_time;
    const uint8_t *data;
    size_t length;
} WagaMedia;

/* Serialize calls for each peer. Input pointers must remain valid for the call.
 * Returned SDP strings must be released with waga_string_destroy.
 * Polled media/transmit pointers belong to the peer and remain valid until the
 * next corresponding poll or peer destruction. Copy them before polling again.
 */
WagaPeer *waga_peer_create(int32_t audio_codec, int32_t video_codec);
WagaPeer *waga_peer_create_with_bwe(int32_t audio_codec, int32_t video_codec,
                                    uint64_t initial_bitrate, uint64_t desired_bitrate);
WagaPeer *waga_receiver_create(void);
void waga_peer_destroy(WagaPeer *peer);
const char *waga_peer_last_error(WagaPeer *peer);
bool waga_peer_add_local_candidate(WagaPeer *peer, const char *address);
bool waga_peer_remove_local_candidate(WagaPeer *peer, const char *address);
bool waga_peer_add_server_reflexive_candidate(
    WagaPeer *peer,
    const char *address,
    const char *base
);
char *waga_peer_create_offer(WagaPeer *peer);
char *waga_peer_create_receive_offer(WagaPeer *peer);
char *waga_peer_accept_offer(WagaPeer *peer, const char *sdp);
void waga_string_destroy(char *value);
bool waga_peer_accept_answer(WagaPeer *peer, const char *sdp);
bool waga_peer_receive(WagaPeer *peer, const char *source, const char *destination,
                       const uint8_t *data, size_t length);
bool waga_peer_handle_timeout(WagaPeer *peer);
uint64_t waga_peer_timeout_millis(WagaPeer *peer);
bool waga_peer_send(WagaPeer *peer, int32_t codec, uint64_t media_time,
                    const uint8_t *data, size_t length);
bool waga_peer_set_desired_bitrate(WagaPeer *peer, uint64_t bitrate);
bool waga_peer_poll_transmit(WagaPeer *peer, WagaTransmit *output);
int32_t waga_peer_poll_event(WagaPeer *peer);
bool waga_peer_poll_media(WagaPeer *peer, WagaMedia *output);
bool waga_peer_poll_bitrate_estimate(WagaPeer *peer, uint64_t *output);

#ifdef __cplusplus
}
#endif

#endif
