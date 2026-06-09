import 'package:flutter/material.dart';

import '../state.dart';
import '../utils.dart';
import '../widgets/data_panel.dart';
import 'screen_helpers.dart';

class SecurityScreen extends StatefulWidget {
  const SecurityScreen({super.key});

  @override
  State<SecurityScreen> createState() => _SecurityScreenState();
}

class _SecurityScreenState extends State<SecurityScreen> {
  final allowlistController = TextEditingController();
  final currentPasswordController = TextEditingController();
  final newPasswordController = TextEditingController();
  final repeatPasswordController = TextEditingController();
  List<Map<String, Object?>> entries = const [];
  bool loading = true;
  bool saving = false;
  bool loadedOnce = false;
  Object? error;

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    if (loadedOnce) return;
    loadedOnce = true;
    Future.microtask(_loadAllowlist);
  }

  @override
  void dispose() {
    allowlistController.dispose();
    currentPasswordController.dispose();
    newPasswordController.dispose();
    repeatPasswordController.dispose();
    super.dispose();
  }

  Future<void> _loadAllowlist() async {
    setState(() {
      loading = true;
      error = null;
    });
    try {
      final payload = await AppScope.of(context)
          .api('GET', '/api/v1/security/machine-allowlist');
      final rows = rowsFrom(payload['entries']);
      entries = rows;
      allowlistController.text = rows.map((row) {
        final cidr = textValue(row['cidr']);
        final description = textValue(row['description']) == '—'
            ? ''
            : textValue(row['description']);
        return description.isEmpty ? cidr : '$cidr $description';
      }).join('\n');
    } catch (cause) {
      error = cause;
    } finally {
      if (mounted) setState(() => loading = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const PageHeader(
          title: '安全设置',
          description: '维护机器 API 白名单，并在 local_password 模式下修改当前用户密码。',
        ),
        if (loading)
          const Surface(
              child: SizedBox(
                  height: 160,
                  child: Center(child: CircularProgressIndicator())))
        else if (error != null)
          Surface(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                const Text('加载失败',
                    style: TextStyle(fontWeight: FontWeight.w700)),
                const SizedBox(height: 8),
                Text(error.toString()),
                const SizedBox(height: 12),
                OutlinedButton.icon(
                    onPressed: _loadAllowlist,
                    icon: const Icon(Icons.refresh),
                    label: const Text('重试')),
              ],
            ),
          )
        else
          _AllowlistEditor(
            entries: entries,
            controller: allowlistController,
            saving: saving,
            onReload: _loadAllowlist,
            onSave: _saveAllowlist,
          ),
        const SizedBox(height: 12),
        _PasswordPanel(
          currentPasswordController: currentPasswordController,
          newPasswordController: newPasswordController,
          repeatPasswordController: repeatPasswordController,
          onSubmit: _changePassword,
        ),
      ],
    );
  }

  Future<void> _saveAllowlist() async {
    final parsed = _parseAllowlist(allowlistController.text);
    final confirmed = await confirmAction(
      context,
      title: '替换机器白名单',
      message: '确认用当前 ${parsed.length} 条规则整体替换服务端机器 API 白名单？',
      confirmLabel: '替换',
      destructive: true,
    );
    if (!confirmed) return;
    if (!mounted) return;
    final controller = AppScope.of(context);
    setState(() => saving = true);
    try {
      await controller.api(
        'PUT',
        '/api/v1/security/machine-allowlist',
        body: {'entries': parsed},
      );
      if (mounted) showResult(context, '白名单已更新');
      await _loadAllowlist();
    } catch (cause) {
      if (mounted) showResult(context, cause.toString());
    } finally {
      if (mounted) setState(() => saving = false);
    }
  }

  Future<void> _changePassword() async {
    final current = currentPasswordController.text;
    final next = newPasswordController.text;
    if (next != repeatPasswordController.text) {
      showResult(context, '两次新密码不一致');
      return;
    }
    if (current.isEmpty || next.isEmpty) {
      showResult(context, '当前密码和新密码不能为空');
      return;
    }
    final confirmed = await confirmAction(
      context,
      title: '修改密码',
      message: '确认修改当前登录用户密码？成功后建议重新登录验证新密码。',
      confirmLabel: '修改',
    );
    if (!confirmed) return;
    if (!mounted) return;
    final controller = AppScope.of(context);
    try {
      await controller.api(
        'POST',
        '/api/v1/auth/change-password',
        body: {
          'current_password': current,
          'new_password': next,
        },
      );
      currentPasswordController.clear();
      newPasswordController.clear();
      repeatPasswordController.clear();
      if (mounted) showResult(context, '密码已修改');
    } catch (cause) {
      if (mounted) showResult(context, cause.toString());
    }
  }

  List<Map<String, Object?>> _parseAllowlist(String text) {
    return text
        .split('\n')
        .map((line) => line.trim())
        .where((line) => line.isNotEmpty)
        .map((line) {
      final parts = line.split(RegExp(r'\s+'));
      return <String, Object?>{
        'cidr': parts.first,
        if (parts.length > 1) 'description': parts.skip(1).join(' '),
      };
    }).toList();
  }
}

class _AllowlistEditor extends StatelessWidget {
  const _AllowlistEditor({
    required this.entries,
    required this.controller,
    required this.saving,
    required this.onReload,
    required this.onSave,
  });

  final List<Map<String, Object?>> entries;
  final TextEditingController controller;
  final bool saving;
  final VoidCallback onReload;
  final VoidCallback onSave;

  @override
  Widget build(BuildContext context) {
    return Surface(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              const Expanded(
                  child: Text('机器 API 白名单',
                      style: TextStyle(fontWeight: FontWeight.w700))),
              IconButton(
                  onPressed: saving ? null : onReload,
                  tooltip: '刷新',
                  icon: const Icon(Icons.refresh)),
              FilledButton.icon(
                onPressed: saving ? null : onSave,
                icon: saving
                    ? const SizedBox(
                        width: 18,
                        height: 18,
                        child: CircularProgressIndicator(strokeWidth: 2))
                    : const Icon(Icons.save),
                label: Text(saving ? '保存中' : '保存'),
              ),
            ],
          ),
          const SizedBox(height: 12),
          TextField(
            controller: controller,
            minLines: 6,
            maxLines: 12,
            decoration: const InputDecoration(
              labelText: '每行一条：CIDR 说明',
              alignLabelWithHint: true,
            ),
          ),
          const SizedBox(height: 16),
          LayoutBuilder(
            builder: (context, constraints) {
              if (constraints.maxWidth < 680) {
                return _CompactAllowlist(entries);
              }
              return SingleChildScrollView(
                scrollDirection: Axis.horizontal,
                child: DataTable(
                  dataRowMinHeight: 52,
                  dataRowMaxHeight: 112,
                  columns: const [
                    DataColumn(label: Text('CIDR')),
                    DataColumn(label: Text('说明')),
                    DataColumn(label: Text('更新时间')),
                  ],
                  rows: entries.map((row) {
                    return DataRow(cells: [
                      DataCell(
                          WrappedTextCell(value: row['cidr'], maxWidth: 220)),
                      DataCell(WrappedTextCell(
                          value: row['description'], maxWidth: 320)),
                      DataCell(WrappedTextCell(
                          value: row['updated_at'], maxWidth: 220)),
                    ]);
                  }).toList(),
                ),
              );
            },
          ),
        ],
      ),
    );
  }
}

class _CompactAllowlist extends StatelessWidget {
  const _CompactAllowlist(this.entries);

  final List<Map<String, Object?>> entries;

  @override
  Widget build(BuildContext context) {
    if (entries.isEmpty) {
      return const SizedBox(
        height: 90,
        child: Center(child: Text('暂无白名单')),
      );
    }
    return Column(
      children: [
        for (var index = 0; index < entries.length; index++) ...[
          _CompactAllowlistItem(entries[index]),
          if (index != entries.length - 1)
            const Divider(height: 24, color: Color(0xffe4e8f0)),
        ],
      ],
    );
  }
}

class _CompactAllowlistItem extends StatelessWidget {
  const _CompactAllowlistItem(this.row);

  final Map<String, Object?> row;

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        SelectableText(
          textValue(row['cidr']),
          style: const TextStyle(fontWeight: FontWeight.w700),
        ),
        const SizedBox(height: 8),
        _AllowlistMeta(label: '说明', value: row['description']),
        const SizedBox(height: 6),
        _AllowlistMeta(label: '更新时间', value: row['updated_at']),
      ],
    );
  }
}

class _AllowlistMeta extends StatelessWidget {
  const _AllowlistMeta({required this.label, required this.value});

  final String label;
  final Object? value;

  @override
  Widget build(BuildContext context) {
    return RichText(
      text: TextSpan(
        style: const TextStyle(color: Color(0xff1d2433), fontSize: 13),
        children: [
          TextSpan(
            text: '$label：',
            style: const TextStyle(
              color: Color(0xff5b6477),
              fontWeight: FontWeight.w600,
            ),
          ),
          TextSpan(text: textValue(value)),
        ],
      ),
      softWrap: true,
    );
  }
}

class _PasswordPanel extends StatelessWidget {
  const _PasswordPanel({
    required this.currentPasswordController,
    required this.newPasswordController,
    required this.repeatPasswordController,
    required this.onSubmit,
  });

  final TextEditingController currentPasswordController;
  final TextEditingController newPasswordController;
  final TextEditingController repeatPasswordController;
  final VoidCallback onSubmit;

  @override
  Widget build(BuildContext context) {
    return Surface(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const Text('修改密码', style: TextStyle(fontWeight: FontWeight.w700)),
          const SizedBox(height: 12),
          Wrap(
            spacing: 12,
            runSpacing: 12,
            crossAxisAlignment: WrapCrossAlignment.center,
            children: [
              SmallTextField(
                  controller: currentPasswordController,
                  label: '当前密码',
                  obscureText: true),
              SmallTextField(
                  controller: newPasswordController,
                  label: '新密码',
                  obscureText: true),
              SmallTextField(
                  controller: repeatPasswordController,
                  label: '重复新密码',
                  obscureText: true),
              FilledButton.icon(
                  onPressed: onSubmit,
                  icon: const Icon(Icons.lock_reset),
                  label: const Text('修改')),
            ],
          ),
        ],
      ),
    );
  }
}
