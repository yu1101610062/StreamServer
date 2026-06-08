import 'dart:convert';
import 'dart:ffi';
import 'dart:io';
import 'dart:isolate';

import 'package:ffi/ffi.dart';

typedef _NativeJsonCall = Pointer<Utf8> Function(Pointer<Utf8>);
typedef _NativeJsonCallDart = Pointer<Utf8> Function(Pointer<Utf8>);
typedef _NativeStringFree = Void Function(Pointer<Utf8>);
typedef _NativeStringFreeDart = void Function(Pointer<Utf8>);

class NativeBridgeError implements Exception {
  NativeBridgeError(this.message, {this.kind, this.status, this.details});

  final String message;
  final String? kind;
  final int? status;
  final Object? details;

  @override
  String toString() => message;
}

class NativeBridge {
  NativeBridge._(this._library)
      : _jsonCall =
            _library.lookupFunction<_NativeJsonCall, _NativeJsonCallDart>(
          'streamserver_desktop_json_call',
        ),
        _stringFree =
            _library.lookupFunction<_NativeStringFree, _NativeStringFreeDart>(
          'streamserver_desktop_string_free',
        );

  final DynamicLibrary _library;
  final _NativeJsonCallDart _jsonCall;
  final _NativeStringFreeDart _stringFree;

  // Keep the dynamic library object alive for the lifetime of the function pointers.
  DynamicLibrary get loadedLibrary => _library;

  static NativeBridge? _instance;

  static NativeBridge get instance {
    _instance ??= NativeBridge._(DynamicLibrary.open(_libraryPath()));
    return _instance!;
  }

  Map<String, Object?> call(String op, Map<String, Object?> payload) {
    final request = <String, Object?>{'op': op, ...payload};
    final input = jsonEncode(request).toNativeUtf8();
    Pointer<Utf8> output = nullptr;
    try {
      output = _jsonCall(input);
      if (output == nullptr) {
        throw NativeBridgeError('native bridge returned a null response');
      }
      final envelope =
          jsonDecode(output.toDartString()) as Map<String, Object?>;
      if (envelope['ok'] == true) {
        return (envelope['data'] as Map?)?.cast<String, Object?>() ??
            <String, Object?>{'value': envelope['data']};
      }
      final error = (envelope['error'] as Map?)?.cast<String, Object?>() ??
          <String, Object?>{};
      throw NativeBridgeError(
        (error['message'] as String?) ?? 'native bridge error',
        kind: error['kind'] as String?,
        status: error['status'] as int?,
        details: error['details'],
      );
    } on ArgumentError catch (error) {
      throw NativeBridgeError(
        'native bridge library is unavailable: $error',
        kind: 'bridge_unavailable',
      );
    } finally {
      calloc.free(input);
      if (output != nullptr) {
        _stringFree(output);
      }
    }
  }

  static Future<Map<String, Object?>> callOnWorker(
      String op, Map<String, Object?> payload) {
    return Isolate.run(() => NativeBridge.instance.call(op, payload));
  }

  static String _libraryPath() {
    final name = _libraryName();
    final override = Platform.environment['STREAMSERVER_DESKTOP_NATIVE_LIB'];
    if (override != null && override.isNotEmpty) {
      return override;
    }
    final candidates = [
      '${Directory.current.path}/build/native/$name',
      '${Directory.current.path}/$name',
      '${File(Platform.resolvedExecutable).parent.path}/$name',
    ];
    for (final candidate in candidates) {
      if (File(candidate).existsSync()) {
        return candidate;
      }
    }
    return name;
  }

  static String _libraryName() {
    if (Platform.isMacOS) return 'libstreamserver_desktop_native.dylib';
    if (Platform.isWindows) return 'streamserver_desktop_native.dll';
    if (Platform.isLinux) return 'libstreamserver_desktop_native.so';
    throw UnsupportedError(
        'StreamServer Desktop supports desktop platforms only');
  }
}
