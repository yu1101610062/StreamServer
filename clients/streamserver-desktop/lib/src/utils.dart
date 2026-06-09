import 'dart:convert';

List<Map<String, Object?>> rowsFrom(Object? value) {
  if (value is List) {
    return value.whereType<Map>().map((item) => item.cast<String, Object?>()).toList();
  }
  if (value is Map) {
    final items = value['items'];
    if (items is List) {
      return items.whereType<Map>().map((item) => item.cast<String, Object?>()).toList();
    }
    final raw = value['value'];
    if (raw is List) {
      return raw.whereType<Map>().map((item) => item.cast<String, Object?>()).toList();
    }
  }
  return const [];
}

String prettyJson(Object? value) {
  const encoder = JsonEncoder.withIndent('  ');
  return encoder.convert(value);
}

String shortId(Object? value) {
  final text = '${value ?? ''}';
  if (text.length <= 12) return text;
  return text.substring(0, 8);
}

String textValue(Object? value) {
  if (value == null) return '—';
  if (value is String && value.isEmpty) return '—';
  return '$value';
}

String bytesLabel(Object? value) {
  final bytes = value is num ? value.toDouble() : double.tryParse('$value') ?? 0;
  if (bytes >= 1024 * 1024 * 1024) return '${(bytes / 1024 / 1024 / 1024).toStringAsFixed(2)} GB';
  if (bytes >= 1024 * 1024) return '${(bytes / 1024 / 1024).toStringAsFixed(2)} MB';
  if (bytes >= 1024) return '${(bytes / 1024).toStringAsFixed(1)} KB';
  return '${bytes.toStringAsFixed(0)} B';
}

String pathQuery(Map<String, Object?> query) {
  final params = query.entries
      .where((entry) => entry.value != null && '${entry.value}'.isNotEmpty)
      .map((entry) => '${entry.key}=${Uri.encodeQueryComponent('${entry.value}')}');
  final joined = params.join('&');
  return joined.isEmpty ? '' : '?$joined';
}
