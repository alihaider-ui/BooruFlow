import 'dart:async';
import 'dart:io';

import 'package:booruflow/features/booru/booru_error_presentation.dart';
import 'package:booruflow/features/booru/services/booru_service.dart';
import 'package:booruflow/features/posts/models/post.dart';
import 'package:booruflow/features/posts/widgets/booru_image.dart';
import 'package:booruflow/intl/translations.g.dart';
import 'package:booruflow/shared/widgets/interaction_controls.dart';
import 'package:booruflow/src/rust/api/booru_download.dart';
import 'package:booruflow/src/rust/api/booru_models.dart';
import 'package:flutter/material.dart';
import 'package:media_kit/media_kit.dart';
import 'package:media_kit_video/media_kit_video.dart';
import 'package:path/path.dart' as path;
import 'package:path_provider/path_provider.dart';

const _videoExtensions = {'mp4', 'webm', 'm4v', 'mov', 'mkv'};

extension PostVideo on Post {
  /// True when the original file is a video (mp4, webm, ...).
  bool get isVideo {
    final media = originalMedia;
    return switch (media.mediaType) {
      MediaType.mp4 || MediaType.webm || MediaType.otherVideo => true,
      _ => _videoExtensions.contains(fileExt.toLowerCase()),
    };
  }
}

/// Plays a post's original video with sound.
///
/// The file is fetched through the Rust engine (same auth, headers, proxy and
/// DNS routing as images), cached in the temp folder, then played locally.
/// Only the [active] page plays, so swiping away stops the sound.
class PostVideoPlayer extends StatefulWidget {
  const PostVideoPlayer({
    super.key,
    required this.post,
    required this.active,
    required this.width,
    required this.height,
  });

  final Post post;
  final bool active;
  final double width;
  final double height;

  @override
  State<PostVideoPlayer> createState() => _PostVideoPlayerState();
}

class _PostVideoPlayerState extends State<PostVideoPlayer> {
  Player? _player;
  VideoController? _controller;
  StreamSubscription<String>? _errors;
  int _loadToken = 0;
  double? _progress; // null = size unknown
  Object? _error;
  bool _started = false;

  @override
  void initState() {
    super.initState();
    if (widget.active) _start();
  }

  @override
  void didUpdateWidget(covariant PostVideoPlayer oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.post.postKey != widget.post.postKey) {
      _reset();
      if (widget.active) _start();
      return;
    }
    if (widget.active && !_started) {
      _start();
    } else if (widget.active) {
      _player?.play();
    } else {
      _player?.pause();
    }
  }

  @override
  void dispose() {
    _reset();
    super.dispose();
  }

  void _reset() {
    _loadToken++;
    _errors?.cancel();
    _errors = null;
    _player?.dispose();
    _player = null;
    _controller = null;
    _started = false;
    _progress = null;
    _error = null;
  }

  Future<void> _start() async {
    _started = true;
    final token = ++_loadToken;
    // Plain assignment: this can run from initState, where setState is not allowed.
    _error = null;
    _progress = null;
    try {
      final file = await _fetch(token);
      if (!mounted || token != _loadToken) return;

      final player = Player();
      final controller = VideoController(player);
      _errors = player.stream.error.listen((message) {
        if (mounted && token == _loadToken) setState(() => _error = message);
      });
      await player.setPlaylistMode(PlaylistMode.loop);
      await player.setVolume(100);
      await player.open(Media(file.path), play: widget.active);
      if (!mounted || token != _loadToken) {
        await player.dispose();
        return;
      }
      setState(() {
        _player = player;
        _controller = controller;
      });
    } catch (error) {
      if (mounted && token == _loadToken) setState(() => _error = error);
    }
  }

  Future<File> _fetch(int token) async {
    final post = widget.post;
    final dir = Directory(
      path.join((await getTemporaryDirectory()).path, 'booruflow_videos'),
    );
    await dir.create(recursive: true);
    final ext = post.fileExt.toLowerCase();
    final name = '${post.sourceId}_${post.id}.${_videoExtensions.contains(ext) ? ext : 'mp4'}'
        .replaceAll(RegExp(r'[^\w.\-]'), '_');
    final file = File(path.join(dir.path, name));
    if (await file.exists() && await file.length() > 0) return file;

    final engine = BooruService.instance.engine;
    final request = await engine.prepareMedia(
      sourceId: post.sourceId,
      postId: post.id,
      variant: MediaVariantKind.original,
    );
    final partial = File('${file.path}.part');
    final sink = partial.openWrite();
    var received = 0;
    try {
      await for (final event in engine
          .streamMedia(request: request)
          .timeout(const Duration(seconds: 60))) {
        if (token != _loadToken) throw const _Cancelled();
        switch (event) {
          case MediaFetchEvent_Failed(:final error):
            throw error;
          case MediaFetchEvent_Chunk(:final bytes, :final totalBytesDecimal):
            sink.add(bytes);
            received += bytes.length;
            final total = int.tryParse(totalBytesDecimal ?? '');
            if (mounted && total != null && total > 0) {
              setState(() => _progress = received / total);
            }
        }
      }
      await sink.close();
      if (received == 0) throw StateError('The video file was empty.');
      return await partial.rename(file.path);
    } catch (_) {
      await sink.close().catchError((_) {});
      if (await partial.exists()) await partial.delete();
      rethrow;
    }
  }

  @override
  Widget build(BuildContext context) {
    final controller = _controller;
    if (controller != null && _error == null) {
      return Video(
        controller: controller,
        width: widget.width,
        height: widget.height,
        fit: BoxFit.contain,
        fill: Colors.transparent,
      );
    }

    final post = widget.post;
    final poster = post.sampleUrl ?? post.previewUrl;
    final colorScheme = Theme.of(context).colorScheme;
    return Stack(
      fit: StackFit.expand,
      children: [
        BooruImage(
          poster,
          sourceId: post.sourceId,
          postId: post.id,
          variant:
              post.variant(MediaVariantKind.sample)?.kind ??
              post.displayMedia.kind,
          width: widget.width,
          height: widget.height,
          fit: BoxFit.contain,
        ),
        ColoredBox(color: Colors.black.withValues(alpha: 0.35)),
        Center(
          child: switch (_error) {
            final Object error when error is! _Cancelled => Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                Tooltip(
                  message: describeBooruError(error),
                  child: Icon(
                    Icons.error_outline,
                    color: colorScheme.onInverseSurface,
                    size: 32,
                  ),
                ),
                const SizedBox(height: 8),
                AppButton(
                  label: t.generic.retry,
                  onPressed: () {
                    setState(_reset);
                    _start();
                  },
                ),
              ],
            ),
            _ when !_started => IconButton.filled(
              iconSize: 40,
              icon: const Icon(Icons.play_arrow_rounded),
              onPressed: () {
                setState(() => _started = true);
                _start();
              },
            ),
            _ => SizedBox.square(
              dimension: 44,
              child: CircularProgressIndicator(
                value: _progress,
                strokeWidth: 3,
                color: colorScheme.onInverseSurface,
              ),
            ),
          },
        ),
      ],
    );
  }
}

class _Cancelled implements Exception {
  const _Cancelled();
}
