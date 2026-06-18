import 'package:flutter_test/flutter_test.dart';

import 'package:streamserver_desktop/src/screens/screen_helpers.dart';
import 'package:streamserver_desktop/src/screens/task_create_screen.dart';
import 'package:streamserver_desktop/src/utils.dart';
import 'package:streamserver_desktop/src/widgets/embedded_player_panel.dart';

void main() {
  test('shortId trims long ids', () {
    expect(shortId('019e95a4-8b44-7983-8a75-2e218e45e21c'), '019e95a4');
  });

  test('rowsFrom reads page items', () {
    expect(
      rowsFrom({
        'items': [
          {'id': 'a'},
        ],
      }).single['id'],
      'a',
    );
  });

  test('cleanQuery drops empty values', () {
    expect(
      cleanQuery(
          {'page': 1, 'keyword': '', 'node_id': null, 'status': 'RUNNING'}),
      {'page': 1, 'status': 'RUNNING'},
    );
  });

  test('playback cache skips live preview mp4 streams', () {
    expect(
      shouldCacheRemoteMediaForPlayback(Uri.parse(
        'http://172.17.13.196/preview/preview-6647aeab.live.mp4',
      )),
      isFalse,
    );
    expect(
      shouldCacheRemoteMediaForPlayback(Uri.parse(
        'http://172.17.13.196/media/uploads/file.mp4',
      )),
      isTrue,
    );
  });

  test('live media detection covers preview path and live query values', () {
    expect(
      isLikelyLiveHttpMedia(Uri.parse(
        'http://172.17.13.196/preview/camera.mp4',
      )),
      isTrue,
    );
    expect(
      isLikelyLiveHttpMedia(Uri.parse(
        'http://172.17.13.196/media/file.mp4?source_mode=live',
      )),
      isTrue,
    );
  });

  test('task input payload includes multicast address fields', () {
    final payload = buildTaskInputPayload(
      inputKind: 'udp_mpegts_multicast',
      sourceMode: 'live',
      loopEnabled: false,
      startOffset: '',
      url: 'udp://239.0.0.1:5000',
      group: '239.0.0.1',
      port: '5000',
      interfaceName: 'en0',
      interfaceIp: '',
      ttl: '16',
    );

    expect(payload['group'], '239.0.0.1');
    expect(payload['port'], 5000);
    expect(payload['interface_name'], 'en0');
    expect(payload['ttl'], 16);
    expect(payload.containsKey('url'), isFalse);
  });

  test('task input payload includes gb rtp port without group', () {
    final payload = buildTaskInputPayload(
      inputKind: 'gb_rtp',
      sourceMode: 'live',
      loopEnabled: false,
      startOffset: '',
      url: 'rtp://127.0.0.1:15060',
      group: '239.0.0.1',
      port: '15060',
      interfaceName: '',
      interfaceIp: '',
      ttl: '',
    );

    expect(payload['port'], 15060);
    expect(payload.containsKey('group'), isFalse);
    expect(payload.containsKey('url'), isFalse);
  });

  test('task input payload keeps url inputs separate from multicast fields',
      () {
    final payload = buildTaskInputPayload(
      inputKind: 'rtsp',
      sourceMode: 'live',
      loopEnabled: false,
      startOffset: '',
      url: 'rtsp://example/live/stream',
      group: '239.0.0.1',
      port: '5000',
      interfaceName: '',
      interfaceIp: '',
      ttl: '',
    );

    expect(payload['url'], 'rtsp://example/live/stream');
    expect(payload.containsKey('group'), isFalse);
    expect(payload.containsKey('port'), isFalse);
  });

  test('task input payload includes vod start offset', () {
    final payload = buildTaskInputPayload(
      inputKind: 'http_mp4',
      sourceMode: 'vod',
      loopEnabled: false,
      startOffset: '600',
      url: 'http://example/video.mp4',
      group: '',
      port: '',
      interfaceName: '',
      interfaceIp: '',
      ttl: '',
    );

    expect(payload['start_offset_sec'], 600);
  });

  test('task input payload omits vod start offset when looping', () {
    final payload = buildTaskInputPayload(
      inputKind: 'http_mp4',
      sourceMode: 'vod',
      loopEnabled: true,
      startOffset: '600',
      url: 'http://example/video.mp4',
      group: '',
      port: '',
      interfaceName: '',
      interfaceIp: '',
      ttl: '',
    );

    expect(payload.containsKey('start_offset_sec'), isFalse);
  });
}
