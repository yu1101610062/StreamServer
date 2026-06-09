import 'package:flutter_test/flutter_test.dart';

import 'package:streamserver_desktop/src/screens/screen_helpers.dart';
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
}
