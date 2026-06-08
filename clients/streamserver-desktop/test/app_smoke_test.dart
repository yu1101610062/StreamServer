import 'package:flutter_test/flutter_test.dart';

import 'package:streamserver_desktop/src/screens/screen_helpers.dart';
import 'package:streamserver_desktop/src/utils.dart';

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
      cleanQuery({'page': 1, 'keyword': '', 'node_id': null, 'status': 'RUNNING'}),
      {'page': 1, 'status': 'RUNNING'},
    );
  });
}
