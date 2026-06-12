import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../core/theme/stream_theme.dart';
import '../state.dart';
import '../widgets/app_select_field.dart';
import '../widgets/window_chrome_bar.dart';
import 'screen_helpers.dart';

const double _loginControlHeight = 48;
const double _discoveryActionWidth = 132;

class LoginScreen extends StatefulWidget {
  const LoginScreen({super.key});

  @override
  State<LoginScreen> createState() => _LoginScreenState();
}

class _LoginScreenState extends State<LoginScreen> {
  final serverController = TextEditingController();
  final usernameController = TextEditingController();
  final passwordController = TextEditingController();
  final manualHostController = TextEditingController();
  final manualPortController = TextEditingController(text: '8080');
  final passwordFocusNode = FocusNode();
  String manualProtocol = 'http';
  List<Map<String, Object?>> discoveredServers = const [];
  bool scanning = false;
  bool probing = false;
  bool unauthenticatedMode = false;
  bool initialServerSeeded = false;
  String scanStatus = '';
  String? error;

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    _seedInitialServer(AppScope.of(context));
  }

  @override
  void dispose() {
    serverController.dispose();
    usernameController.dispose();
    passwordController.dispose();
    manualHostController.dispose();
    manualPortController.dispose();
    passwordFocusNode.dispose();
    super.dispose();
  }

  void _seedInitialServer(AppController controller) {
    if (initialServerSeeded) return;
    final baseUrl = controller.server?.baseUrl.trim();
    if (baseUrl == null || baseUrl.isEmpty) return;
    if (serverController.text.trim().isEmpty) {
      serverController.text = baseUrl;
    }
    initialServerSeeded = true;
  }

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    final profiles = controller.serverProfiles;
    final colors = context.streamColors;
    return Scaffold(
      backgroundColor: colors.appBackground,
      body: Column(
        children: [
          const WindowChromeBar(),
          Expanded(
            child: LayoutBuilder(
              builder: (context, constraints) {
                final compact = constraints.maxWidth < 560;
                final outerPadding = compact ? 12.0 : 24.0;
                final cardPadding = compact ? 18.0 : 28.0;
                return DecoratedBox(
                  decoration: BoxDecoration(color: colors.appBackground),
                  child: SafeArea(
                    child: SingleChildScrollView(
                      padding: EdgeInsets.all(outerPadding),
                      child: ConstrainedBox(
                        constraints: BoxConstraints(
                          minHeight: math.max(
                              0, constraints.maxHeight - outerPadding * 2),
                        ),
                        child: Center(
                          child: ConstrainedBox(
                            constraints: const BoxConstraints(maxWidth: 880),
                            child: DecoratedBox(
                              decoration: BoxDecoration(
                                color: colors.surface,
                                border: Border.all(color: colors.border),
                                borderRadius: BorderRadius.circular(14),
                                boxShadow: [
                                  BoxShadow(
                                    color: Colors.black.withValues(alpha: 0.28),
                                    blurRadius: 28,
                                    offset: const Offset(0, 18),
                                  ),
                                ],
                              ),
                              child: Padding(
                                padding: EdgeInsets.all(cardPadding),
                                child: DefaultTextStyle(
                                  style: TextStyle(color: colors.textPrimary),
                                  child: Align(
                                    alignment: Alignment.center,
                                    child: ConstrainedBox(
                                      constraints:
                                          const BoxConstraints(maxWidth: 640),
                                      child: Column(
                                        mainAxisSize: MainAxisSize.min,
                                        crossAxisAlignment:
                                            CrossAxisAlignment.stretch,
                                        children: [
                                          Text(
                                            'STREAMSERVER',
                                            style: TextStyle(
                                              color: colors.primary,
                                              fontWeight: FontWeight.w800,
                                              letterSpacing: 1.2,
                                            ),
                                          ),
                                          const SizedBox(height: 8),
                                          Text(
                                            '桌面控制台',
                                            style: Theme.of(context)
                                                .textTheme
                                                .headlineMedium
                                                ?.copyWith(
                                                  color: colors.textPrimary,
                                                  fontWeight: FontWeight.w900,
                                                ),
                                          ),
                                          const SizedBox(height: 24),
                                          _ServerAddressField(
                                            controller: serverController,
                                            profiles: profiles,
                                            textInputAction: unauthenticatedMode
                                                ? TextInputAction.done
                                                : TextInputAction.next,
                                            onSubmitted: unauthenticatedMode
                                                ? (_) =>
                                                    _submitLogin(controller)
                                                : null,
                                            onProfileSelected: (profile) {
                                              controller.selectServer(profile);
                                              serverController.text =
                                                  profile.baseUrl;
                                            },
                                          ),
                                          const SizedBox(height: 12),
                                          _DiscoveryPanel(
                                            scanning: scanning,
                                            probing: probing,
                                            status: scanStatus,
                                            results: discoveredServers,
                                            manualProtocol: manualProtocol,
                                            manualHostController:
                                                manualHostController,
                                            manualPortController:
                                                manualPortController,
                                            onProtocolChanged: (value) =>
                                                setState(() =>
                                                    manualProtocol = value),
                                            onScan: () => _scan(controller),
                                            onSelect: (baseUrl) => setState(
                                                () => serverController.text =
                                                    baseUrl),
                                            onProbe: () => _probe(controller),
                                          ),
                                          const SizedBox(height: 12),
                                          _AuthModeSelector(
                                            unauthenticatedMode:
                                                unauthenticatedMode,
                                            onChanged: (value) {
                                              setState(() {
                                                unauthenticatedMode = value;
                                                error = null;
                                              });
                                              if (value) {
                                                FocusScope.of(context)
                                                    .unfocus();
                                              }
                                            },
                                          ),
                                          if (!unauthenticatedMode) ...[
                                            const SizedBox(height: 12),
                                            TextField(
                                              controller: usernameController,
                                              textInputAction:
                                                  TextInputAction.next,
                                              decoration: const InputDecoration(
                                                labelText: '用户名',
                                                prefixIcon:
                                                    Icon(LucideIcons.user),
                                              ),
                                              onSubmitted: (_) =>
                                                  passwordFocusNode
                                                      .requestFocus(),
                                            ),
                                            const SizedBox(height: 12),
                                            TextField(
                                              controller: passwordController,
                                              focusNode: passwordFocusNode,
                                              obscureText: true,
                                              textInputAction:
                                                  TextInputAction.done,
                                              decoration: const InputDecoration(
                                                labelText: '密码',
                                                prefixIcon:
                                                    Icon(LucideIcons.lock),
                                              ),
                                              onSubmitted: (_) =>
                                                  _submitLogin(controller),
                                            ),
                                          ],
                                          if (error != null) ...[
                                            const SizedBox(height: 14),
                                            InlineStatusMessage(
                                              message: error!,
                                              tone: InlineStatusTone.danger,
                                            ),
                                          ],
                                          const SizedBox(height: 20),
                                          FilledButton.icon(
                                            onPressed: controller.busy
                                                ? null
                                                : () =>
                                                    _submitLogin(controller),
                                            icon: controller.busy
                                                ? const SizedBox.square(
                                                    dimension: 16,
                                                    child:
                                                        CircularProgressIndicator(
                                                      strokeWidth: 2,
                                                    ),
                                                  )
                                                : const Icon(LucideIcons.logIn),
                                            label: Text(unauthenticatedMode
                                                ? '连接'
                                                : '连接并登录'),
                                          ),
                                        ],
                                      ),
                                    ),
                                  ),
                                ),
                              ),
                            ),
                          ),
                        ),
                      ),
                    ),
                  ),
                );
              },
            ),
          ),
        ],
      ),
    );
  }

  Future<void> _submitLogin(AppController controller) async {
    if (controller.busy) return;
    await _login(controller);
  }

  Future<void> _login(AppController controller) async {
    try {
      setState(() => error = null);
      final baseUrl = serverController.text.trim();
      final username = usernameController.text.trim();
      if (baseUrl.isEmpty) {
        setState(() => error = '请填写 Core 地址，或先选择/探测服务器。');
        return;
      }
      if (!baseUrl.startsWith('http://') && !baseUrl.startsWith('https://')) {
        setState(() => error = 'Core 地址需要包含 http:// 或 https://。');
        return;
      }
      if (!unauthenticatedMode && username.isEmpty) {
        setState(() => error = '请填写用户名。');
        return;
      }
      if (unauthenticatedMode) {
        await controller.loginWithoutAuth(baseUrl: baseUrl);
      } else {
        await controller.login(
          baseUrl: baseUrl,
          username: username,
          password: passwordController.text,
        );
      }
    } catch (cause) {
      setState(() {
        error = cause.toString();
      });
    }
  }

  Future<void> _scan(AppController controller) async {
    setState(() {
      scanning = true;
      error = null;
      discoveredServers = const [];
      scanStatus = manualHostController.text.trim().isEmpty
          ? '正在扫描已保存服务器和局域网'
          : '正在优先探测手动地址和已保存服务器';
    });
    try {
      if (manualHostController.text.trim().isNotEmpty) {
        final quick = await _probeManual(controller);
        if (quick != null && mounted) {
          final baseUrl = quick['base_url'] as String;
          setState(() {
            serverController.text = baseUrl;
            discoveredServers = [quick];
            scanStatus = '已发现 $baseUrl，继续扫描相关网段';
          });
        }
      }
      final results = await controller.scanServers(
        baseUrls: _scanBaseUrls(controller),
        seedHosts: _scanSeedHosts(),
      );
      setState(() {
        discoveredServers =
            _mergeDiscoveryResults([...discoveredServers, ...results]);
        scanStatus = discoveredServers.isEmpty
            ? ''
            : '扫描完成，发现 ${discoveredServers.length} 个实例';
        if (discoveredServers.isEmpty) {
          error = '未扫描到 StreamServer Core，可使用手动连接。';
        }
      });
    } catch (cause) {
      setState(() => error = cause.toString());
    } finally {
      if (mounted) setState(() => scanning = false);
    }
  }

  Future<void> _probe(AppController controller) async {
    setState(() {
      probing = true;
      error = null;
      scanStatus = '正在探测手动地址';
    });
    try {
      final item = await _probeManual(controller);
      if (item == null) {
        return;
      }
      final baseUrl = item['base_url'] as String;
      setState(() {
        serverController.text = baseUrl;
        discoveredServers =
            _mergeDiscoveryResults([item, ...discoveredServers]);
        scanStatus = '已发现 $baseUrl';
      });
    } catch (cause) {
      setState(() => error = cause.toString());
    } finally {
      if (mounted) setState(() => probing = false);
    }
  }

  Future<Map<String, Object?>?> _probeManual(AppController controller) async {
    final host = manualHostController.text.trim();
    if (host.isEmpty) {
      if (mounted) setState(() => error = '请填写要探测的 IP 或域名。');
      return null;
    }
    final port = int.tryParse(manualPortController.text) ?? 8080;
    final result = await controller.probeServer(
      protocol: manualProtocol,
      host: host,
      port: port,
    );
    final found = result['found'] == true;
    if (!found) {
      final detail = result['error'] as String?;
      if (mounted) {
        setState(() => error = detail == null || detail.isEmpty
            ? '该地址未识别为 StreamServer Core'
            : '探测失败：$detail');
      }
      return null;
    }
    return (result['item'] as Map).cast<String, Object?>();
  }

  List<String> _scanBaseUrls(AppController controller) {
    final host = manualHostController.text.trim();
    final values = <String>{
      serverController.text.trim(),
      ...controller.serverProfiles.map((profile) => profile.baseUrl.trim()),
    };
    if (host.isNotEmpty) {
      values.add(
          '${manualProtocol.trim()}://$host:${int.tryParse(manualPortController.text) ?? 8080}');
    }
    return values
        .where((value) =>
            value.startsWith('http://') || value.startsWith('https://'))
        .toList();
  }

  List<String> _scanSeedHosts() {
    final host = manualHostController.text.trim();
    return host.isEmpty ? const [] : [host];
  }

  List<Map<String, Object?>> _mergeDiscoveryResults(
      List<Map<String, Object?>> values) {
    final merged = <String, Map<String, Object?>>{};
    for (final item in values) {
      final baseUrl = item['base_url'] as String?;
      if (baseUrl == null || baseUrl.isEmpty) continue;
      merged[baseUrl] = item;
    }
    return merged.values.toList()
      ..sort((left, right) =>
          '${left['base_url']}'.compareTo('${right['base_url']}'));
  }
}

class _ServerAddressField extends StatelessWidget {
  const _ServerAddressField({
    required this.controller,
    required this.profiles,
    required this.textInputAction,
    required this.onSubmitted,
    required this.onProfileSelected,
  });

  final TextEditingController controller;
  final List<ServerProfile> profiles;
  final TextInputAction textInputAction;
  final ValueChanged<String>? onSubmitted;
  final ValueChanged<ServerProfile> onProfileSelected;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final fillColor = Theme.of(context).inputDecorationTheme.fillColor!;
    return _FloatingLabelFrame(
      label: 'Core 地址',
      labelBackground: colors.surface,
      fillColor: fillColor,
      child: Row(
        children: [
          SizedBox(
            width: 42,
            child: Icon(
              LucideIcons.server,
              size: 19,
              color: colors.textSecondary,
            ),
          ),
          Expanded(
            child: TextField(
              controller: controller,
              textInputAction: textInputAction,
              style: TextStyle(color: colors.textPrimary, fontSize: 14),
              decoration: InputDecoration(
                border: InputBorder.none,
                enabledBorder: InputBorder.none,
                focusedBorder: InputBorder.none,
                disabledBorder: InputBorder.none,
                filled: false,
                isDense: true,
                contentPadding: EdgeInsets.zero,
                hintText: 'http(s)://主机:端口',
                hintStyle: TextStyle(color: colors.textMuted, fontSize: 13),
              ),
              onSubmitted: onSubmitted,
            ),
          ),
          if (profiles.isNotEmpty)
            Builder(
              builder: (context) {
                final anchorController = MenuController();
                return MenuAnchor(
                  controller: anchorController,
                  alignmentOffset: const Offset(-224, 8),
                  style: streamMenuStyle(context, minWidth: 320),
                  menuChildren: [
                    for (final profile in profiles)
                      StreamMenuOption(
                        width: 320,
                        label: profile.name.isEmpty
                            ? profile.baseUrl
                            : profile.name,
                        subtitle: profile.name != profile.baseUrl
                            ? profile.baseUrl
                            : null,
                        icon: profile.baseUrl == controller.text.trim()
                            ? LucideIcons.check
                            : LucideIcons.database,
                        selected: profile.baseUrl == controller.text.trim(),
                        onPressed: () {
                          anchorController.close();
                          onProfileSelected(profile);
                        },
                      ),
                  ],
                  builder: (context, menuController, child) {
                    return _SavedServerButton(
                      open: menuController.isOpen,
                      onPressed: () {
                        if (menuController.isOpen) {
                          menuController.close();
                        } else {
                          menuController.open();
                        }
                      },
                    );
                  },
                );
              },
            ),
        ],
      ),
    );
  }
}

class _SavedServerButton extends StatelessWidget {
  const _SavedServerButton({
    required this.open,
    required this.onPressed,
  });

  final bool open;
  final VoidCallback onPressed;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Tooltip(
      message: '选择已保存服务器',
      waitDuration: const Duration(milliseconds: 450),
      child: SizedBox(
        width: 112,
        height: _loginControlHeight,
        child: DecoratedBox(
          decoration: BoxDecoration(
            color: open
                ? colors.primary
                    .withValues(alpha: context.isDarkMode ? 0.18 : 0.08)
                : colors.surfaceAlt,
            border: Border(
              left: BorderSide(
                color: open ? colors.primary : colors.border,
                width: open ? 1.2 : 1,
              ),
            ),
            borderRadius: const BorderRadius.only(
              topRight: Radius.circular(7),
              bottomRight: Radius.circular(7),
            ),
          ),
          child: Material(
            color: Colors.transparent,
            borderRadius: const BorderRadius.only(
              topRight: Radius.circular(7),
              bottomRight: Radius.circular(7),
            ),
            child: InkWell(
              borderRadius: const BorderRadius.only(
                topRight: Radius.circular(7),
                bottomRight: Radius.circular(7),
              ),
              onTap: onPressed,
              child: Row(
                mainAxisAlignment: MainAxisAlignment.center,
                children: [
                  Icon(
                    LucideIcons.database,
                    size: 15,
                    color: open ? colors.primary : colors.textSecondary,
                  ),
                  const SizedBox(width: 6),
                  Text(
                    '已保存',
                    style: TextStyle(
                        color: open ? colors.primary : colors.textSecondary,
                        fontSize: 12,
                        fontWeight: FontWeight.w800),
                  ),
                  const SizedBox(width: 4),
                  Icon(
                    open ? LucideIcons.chevronUp : LucideIcons.chevronDown,
                    size: 14,
                    color: open ? colors.primary : colors.textSecondary,
                  ),
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}

class _FloatingLabelFrame extends StatelessWidget {
  const _FloatingLabelFrame({
    required this.label,
    required this.labelBackground,
    required this.fillColor,
    required this.child,
  });

  final String label;
  final Color labelBackground;
  final Color fillColor;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return SizedBox(
      height: _loginControlHeight,
      child: Stack(
        clipBehavior: Clip.none,
        children: [
          Positioned.fill(
            child: DecoratedBox(
              decoration: BoxDecoration(
                color: fillColor,
                border: Border.all(color: colors.border),
                borderRadius: BorderRadius.circular(8),
              ),
              child: child,
            ),
          ),
          Positioned(
            left: 12,
            top: -7,
            child: DecoratedBox(
              decoration: BoxDecoration(color: labelBackground),
              child: Padding(
                padding: const EdgeInsets.symmetric(horizontal: 4),
                child: Text(
                  label,
                  style: TextStyle(
                    color: colors.textSecondary,
                    fontSize: 12,
                    height: 1,
                  ),
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class _DiscoveryTextField extends StatelessWidget {
  const _DiscoveryTextField({
    required this.controller,
    required this.label,
    required this.hintText,
    required this.enabled,
    this.keyboardType,
  });

  final TextEditingController controller;
  final String label;
  final String hintText;
  final bool enabled;
  final TextInputType? keyboardType;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final fillColor = Theme.of(context).inputDecorationTheme.fillColor!;
    return _FloatingLabelFrame(
      label: label,
      labelBackground: colors.surfaceAlt,
      fillColor: fillColor,
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 12),
        child: Center(
          child: TextField(
            controller: controller,
            enabled: enabled,
            keyboardType: keyboardType,
            style: TextStyle(color: colors.textPrimary, fontSize: 14),
            decoration: InputDecoration(
              border: InputBorder.none,
              enabledBorder: InputBorder.none,
              focusedBorder: InputBorder.none,
              disabledBorder: InputBorder.none,
              filled: false,
              isDense: true,
              contentPadding: EdgeInsets.zero,
              hintText: hintText,
              hintStyle: TextStyle(color: colors.textMuted, fontSize: 13),
            ),
          ),
        ),
      ),
    );
  }
}

class _AuthModeSelector extends StatelessWidget {
  const _AuthModeSelector({
    required this.unauthenticatedMode,
    required this.onChanged,
  });

  final bool unauthenticatedMode;
  final ValueChanged<bool> onChanged;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return DecoratedBox(
      decoration: BoxDecoration(
        color: colors.surfaceAlt.withValues(alpha: 0.72),
        border: Border.all(color: colors.border),
        borderRadius: BorderRadius.circular(8),
      ),
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text(
              '登录模式',
              style: TextStyle(
                color: colors.textPrimary,
                fontWeight: FontWeight.w800,
              ),
            ),
            const SizedBox(height: 10),
            LayoutBuilder(
              builder: (context, constraints) {
                final narrow = constraints.maxWidth < 460;
                final accountOption = _AuthModeOption(
                  selected: !unauthenticatedMode,
                  icon: LucideIcons.keyRound,
                  title: '账号密码登录',
                  subtitle: '需要用户名和密码',
                  onPressed: () => onChanged(false),
                );
                final noAuthOption = _AuthModeOption(
                  selected: unauthenticatedMode,
                  icon: LucideIcons.shieldOff,
                  title: '无认证连接',
                  subtitle: '服务端关闭鉴权时使用',
                  onPressed: () => onChanged(true),
                );
                if (narrow) {
                  return Column(
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      accountOption,
                      const SizedBox(height: 8),
                      noAuthOption,
                    ],
                  );
                }
                return Row(
                  children: [
                    Expanded(child: accountOption),
                    const SizedBox(width: 8),
                    Expanded(child: noAuthOption),
                  ],
                );
              },
            ),
          ],
        ),
      ),
    );
  }
}

class _AuthModeOption extends StatelessWidget {
  const _AuthModeOption({
    required this.selected,
    required this.icon,
    required this.title,
    required this.subtitle,
    required this.onPressed,
  });

  final bool selected;
  final IconData icon;
  final String title;
  final String subtitle;
  final VoidCallback onPressed;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final borderColor = selected ? colors.primary : colors.border;
    final backgroundColor = selected
        ? colors.primary.withValues(alpha: context.isDarkMode ? 0.18 : 0.08)
        : colors.surface;
    final iconColor = selected ? colors.primary : colors.textSecondary;
    return Material(
      color: Colors.transparent,
      child: InkWell(
        borderRadius: BorderRadius.circular(8),
        onTap: onPressed,
        child: DecoratedBox(
          decoration: BoxDecoration(
            color: backgroundColor,
            border: Border.all(
              color: borderColor,
              width: selected ? 1.4 : 1,
            ),
            borderRadius: BorderRadius.circular(8),
          ),
          child: Padding(
            padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 10),
            child: Row(
              children: [
                Icon(icon, size: 18, color: iconColor),
                const SizedBox(width: 10),
                Expanded(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text(
                        title,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: TextStyle(
                          color: colors.textPrimary,
                          fontSize: 13,
                          fontWeight: FontWeight.w800,
                        ),
                      ),
                      const SizedBox(height: 2),
                      Text(
                        subtitle,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: TextStyle(
                          color: colors.textSecondary,
                          fontSize: 12,
                        ),
                      ),
                    ],
                  ),
                ),
                const SizedBox(width: 8),
                Icon(
                  selected ? LucideIcons.circleCheck : LucideIcons.circle,
                  size: 17,
                  color: iconColor,
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

class _ProtocolSelector extends StatelessWidget {
  const _ProtocolSelector({
    required this.value,
    required this.enabled,
    required this.onChanged,
  });

  final String value;
  final bool enabled;
  final ValueChanged<String> onChanged;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return SizedBox(
      height: _loginControlHeight,
      child: DecoratedBox(
        decoration: BoxDecoration(
          color: Theme.of(context).inputDecorationTheme.fillColor,
          border: Border.all(color: colors.border),
          borderRadius: BorderRadius.circular(8),
        ),
        child: Row(
          children: [
            _ProtocolOption(
              label: 'http',
              selected: value == 'http',
              enabled: enabled,
              onPressed: () => onChanged('http'),
            ),
            Container(width: 1, color: colors.border),
            _ProtocolOption(
              label: 'https',
              selected: value == 'https',
              enabled: enabled,
              onPressed: () => onChanged('https'),
            ),
          ],
        ),
      ),
    );
  }
}

class _ProtocolOption extends StatelessWidget {
  const _ProtocolOption({
    required this.label,
    required this.selected,
    required this.enabled,
    required this.onPressed,
  });

  final String label;
  final bool selected;
  final bool enabled;
  final VoidCallback onPressed;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final foreground = selected ? colors.primary : colors.textSecondary;
    return Expanded(
      child: Material(
        color: selected
            ? colors.primary.withValues(alpha: context.isDarkMode ? 0.18 : 0.08)
            : Colors.transparent,
        borderRadius: BorderRadius.circular(7),
        child: InkWell(
          borderRadius: BorderRadius.circular(7),
          onTap: enabled ? onPressed : null,
          child: Center(
            child: Text(
              label,
              style: TextStyle(
                color: enabled ? foreground : colors.textMuted,
                fontSize: 13,
                fontWeight: selected ? FontWeight.w800 : FontWeight.w700,
              ),
            ),
          ),
        ),
      ),
    );
  }
}

class _DiscoveryPanel extends StatelessWidget {
  const _DiscoveryPanel({
    required this.scanning,
    required this.probing,
    required this.status,
    required this.results,
    required this.manualProtocol,
    required this.manualHostController,
    required this.manualPortController,
    required this.onProtocolChanged,
    required this.onScan,
    required this.onSelect,
    required this.onProbe,
  });

  final bool scanning;
  final bool probing;
  final String status;
  final List<Map<String, Object?>> results;
  final String manualProtocol;
  final TextEditingController manualHostController;
  final TextEditingController manualPortController;
  final ValueChanged<String> onProtocolChanged;
  final VoidCallback onScan;
  final ValueChanged<String> onSelect;
  final VoidCallback onProbe;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return LayoutBuilder(
      builder: (context, constraints) {
        final narrow = constraints.maxWidth < 560;
        final scanButton = SizedBox(
          width: narrow ? double.infinity : _discoveryActionWidth,
          height: _loginControlHeight,
          child: OutlinedButton.icon(
            onPressed: scanning ? null : onScan,
            icon: scanning
                ? const SizedBox.square(
                    dimension: 16,
                    child: CircularProgressIndicator(strokeWidth: 2),
                  )
                : const Icon(LucideIcons.radar),
            label: Text(scanning ? '扫描中' : '扫描局域网'),
          ),
        );
        final protocolField = _ProtocolSelector(
          value: manualProtocol,
          enabled: !scanning && !probing,
          onChanged: onProtocolChanged,
        );
        final hostField = _DiscoveryTextField(
          controller: manualHostController,
          enabled: !scanning && !probing,
          label: 'IP / 域名',
          hintText: '主机名或 IP 地址',
        );
        final portField = _DiscoveryTextField(
          controller: manualPortController,
          enabled: !scanning && !probing,
          label: '端口',
          hintText: '',
          keyboardType: TextInputType.number,
        );
        final probeButton = FilledButton.icon(
          onPressed: scanning || probing ? null : onProbe,
          icon: probing
              ? const SizedBox.square(
                  dimension: 16,
                  child: CircularProgressIndicator(strokeWidth: 2),
                )
              : const Icon(LucideIcons.searchCheck),
          label: Text(probing ? '探测中' : '探测'),
        );
        return DecoratedBox(
          decoration: BoxDecoration(
            color: colors.surfaceAlt.withValues(alpha: 0.72),
            border: Border.all(color: colors.border),
            borderRadius: BorderRadius.circular(8),
          ),
          child: Padding(
            padding: const EdgeInsets.all(12),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                if (narrow) ...[
                  Text(
                    '服务器发现',
                    style: TextStyle(
                      color: colors.textPrimary,
                      fontWeight: FontWeight.w800,
                    ),
                  ),
                  const SizedBox(height: 8),
                  scanButton,
                ] else
                  Row(
                    children: [
                      Expanded(
                        child: Text(
                          '服务器发现',
                          style: TextStyle(
                            color: colors.textPrimary,
                            fontWeight: FontWeight.w800,
                          ),
                        ),
                      ),
                      scanButton,
                    ],
                  ),
                const SizedBox(height: 12),
                if (narrow)
                  Column(
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      protocolField,
                      const SizedBox(height: 8),
                      SizedBox(
                        height: _loginControlHeight,
                        child: hostField,
                      ),
                      const SizedBox(height: 8),
                      SizedBox(
                        height: _loginControlHeight,
                        child: portField,
                      ),
                      const SizedBox(height: 8),
                      SizedBox(
                        height: _loginControlHeight,
                        child: probeButton,
                      ),
                    ],
                  )
                else
                  Row(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      SizedBox(
                        width: 118,
                        height: _loginControlHeight,
                        child: protocolField,
                      ),
                      const SizedBox(width: 8),
                      Expanded(
                        child: SizedBox(
                          height: _loginControlHeight,
                          child: hostField,
                        ),
                      ),
                      const SizedBox(width: 8),
                      SizedBox(
                        width: 104,
                        height: _loginControlHeight,
                        child: portField,
                      ),
                      const SizedBox(width: 8),
                      SizedBox(
                        width: _discoveryActionWidth,
                        height: _loginControlHeight,
                        child: probeButton,
                      ),
                    ],
                  ),
                if (scanning || status.isNotEmpty) ...[
                  const SizedBox(height: 12),
                  if (scanning) const LinearProgressIndicator(minHeight: 3),
                  if (status.isNotEmpty) ...[
                    const SizedBox(height: 8),
                    Text(
                      status,
                      style: TextStyle(
                        color: colors.textSecondary,
                        fontSize: 12,
                      ),
                    ),
                  ],
                ],
                if (results.isNotEmpty) ...[
                  const SizedBox(height: 12),
                  ConstrainedBox(
                    constraints: const BoxConstraints(maxHeight: 150),
                    child: SingleChildScrollView(
                      child: Wrap(
                        spacing: 8,
                        runSpacing: 8,
                        children: results.map((row) {
                          final baseUrl = '${row['base_url']}';
                          return ActionChip(
                            avatar: const Icon(LucideIcons.server, size: 18),
                            label: Text(
                                '$baseUrl · ${row['latency_ms'] ?? '-'}ms'),
                            onPressed: () => onSelect(baseUrl),
                          );
                        }).toList(),
                      ),
                    ),
                  ),
                ],
              ],
            ),
          ),
        );
      },
    );
  }
}
