import { createRouter, createWebHistory } from 'vue-router'

// The session guard arrives with /api/me; until then every view is reachable
// and empty.
const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: '/', name: 'home', component: () => import('@/views/HomeView.vue') },
    {
      path: '/login',
      name: 'login',
      component: () => import('@/views/LoginView.vue'),
      meta: { public: true },
    },
    {
      path: '/connections',
      name: 'connections',
      component: () => import('@/views/ConnectionsView.vue'),
    },
    { path: '/tokens', name: 'tokens', component: () => import('@/views/TokensView.vue') },
    { path: '/activity', name: 'activity', component: () => import('@/views/ActivityView.vue') },
    {
      path: '/:pathMatch(.*)*',
      name: 'not-found',
      component: () => import('@/views/NotFoundView.vue'),
    },
  ],
})

export default router
